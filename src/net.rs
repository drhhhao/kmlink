//! 网络：一个会话 = 一条 TCP（可靠、加密、分帧）+ 一个 UDP 端口（鼠标位置）。
//!
//! 线程：
//! - listener：TcpListener 绑 0.0.0.0:cfg.port；dialer：cfg.peer 有值且当前没有会话时，每 2 秒（加 0~500ms 抖动）连一次，超时 2 秒。
//!   对方地址先按 IP 字面量解析（裸 IPv6 用默认端口）；套接字只有 IPv4，只解析出 IPv6 时明确报错。
//! - 每个会话：reader 线程（读帧、分发；绝不写 socket）。
//! - net-out（常驻，独占 hub 的 net 通道）：分拣要发的东西。鼠标位置由它直接发 UDP，不经过 TCP 写路径（传图时也不受影响）；
//!   两次至少隔 2ms，只发最新的，间隔一到立刻补发最新值（不丢最后一个位置）。其余进可靠队列。每秒 Ping / UDP Hi / 检查。
//! - net-tcp（常驻）：从可靠队列取：先发全部消息，再发至多一个 16 KiB 图片分片，如此循环 —— 按键等消息最多等一个分片。
//!   图片 / 大段文字是压缩流（hub::clip_stream）：压缩线程边压缩边交分片，交来一片就叫醒 net-tcp；
//!   net-tcp 发走一片，压缩线程才能再交（未发出的至多 1 MiB）。流被丢弃（CancelClip、新内容、会话断开）时压缩线程立刻停下。
//! - udp：UdpSocket 绑 0.0.0.0:cfg.port（失败则每 5 秒重试；SO_RCVBUF 256 KiB），一个接收线程。
//!
//! 握手见 crypto.rs（KML2：预共享密钥 + X25519 临时密钥，前向保密）。握手 + 第一帧有 2 秒绝对期限，第一帧（认证前）≤ 64 KiB。
//! 认证前的入站连接：每个源 IP 同时至多 2 个，总共至多 8 个（cfg.peer 的 IP 和上次连上的 IP 总数满了也放进来）。
//! 握手后客户端先发 Hello（sys::monitors_dpi()、cfg.name、udp_port、id = proto::instance_id()）+ Layout；
//! 服务端验证这第一帧（= 对方有密钥）之后才发自己的 Hello + Layout —— 认证前服务端只发随机数和临时公钥。
//! 收到旧版魔数 KML1：报“对方版本太旧，请两台都更新”。
//! 连到自己：dialer 记住最近 10 秒发出的 nonce_c，listener 收到相同的 nonce_c 立即拒绝（认证前就识别，也挡住反射）；
//!   Hello.id 等于本进程 id 同样算。报“对方地址指向本机”，60 秒内不再拨同一个 cfg.peer；任何会话建立后解除。
//! TCP：set_nodelay(true)。读超时 6 秒；每 1 秒发 Ping；6 秒收不到任何帧就断开。
//!   1.5 秒收不到任何帧：injector.stale（会话不断，只抬起按着的键和按钮，防卡键）。
//! 同时只保留一个会话：新会话认证成功时，若旧会话不到 2 秒，保留 nonce_c 字典序更大的那个（两边会选同一个）；
//! 若旧会话连着另一台（Hello.id 不同）且仍活跃，拒绝新来的；否则用新的替换旧的。
//! 登记/结束会话时 injector 和 input 的通知都在 holder 锁内发出，并发时顺序一致。
//! 密钥/对方地址/端口变了（每秒检查）→ 结束当前会话，用新设置重连。
//! “密钥不一致”“版本太旧”这类认证前的错误：只有当前没有会话、且来自 cfg.peer / 上次连上的 IP（或没填 cfg.peer）时才显示在状态里。
//!
//! 收到的消息：
//!   Hello → 清洗显示器和 DPI（proto::sanitize_desk，清洗后为空 = 没有显示器）；config::save_peer_desk（带 dpis）；
//!     hub.to_input(PeerUp{name, desk})；hub.set_status(connected=true, peer_name, peer_addr)
//!   Layout{rev 比本机新} → 镜像后只改 hub.cfg 的排布，config::update 只写排布字段，再 hub.to_input(Config(..))
//!     rev 相同但排布不一致：instance id 较小的一方采用对方的（确定性，不回发）。rev 超前本机时间 1 天以上、数值离谱的不要。
//!     peer.json / config.toml 内容变了才写，每秒至多一次（来得太快就攒着，下一帧或会话结束时写最新的）。
//!   Enter/Leave/Button/Wheel/Key/Move → injector
//!   Yield → hub.to_input(Yield)
//!   ClipText → hub.to_desktop(ClipText)；ClipStart → 按 raw_len（受上限管）GlobalAlloc 好最终的那块内存，
//!     之后每个分片直接解压进去（不环绕的输出缓冲），同时边解边算摘要、图片一收到头就校验 DIB；
//!     结束帧到了核对压缩总长、解压长度，做完整的 DIB 校验 → hub.to_desktop(ClipData)，desktop 直接交给剪贴板。
//!     全程只有 raw_len 这一份（另有约 10 KiB 的解压器状态）。
//!     format = 13（CF_UNICODETEXT，大段文字）受 clipboard.enabled 管，图片另受 clipboard.images 管。
//!     解压出错、超长、分片错位、头不对、对方作废（FRAME_ABORT）、30 秒没进展：立刻释放。
//!     Out::CancelClip 时丢掉正在接收的（之后这个 id 的分片都忽略）；新的 ClipStart 顶掉旧的。
//!   Ping → 经可靠队列回 Pong
//!   Rename{name} → hub.to_desktop(Rename)（desktop 清理名字后写进 config.toml）
//! 会话结束：injector.end_session；hub.to_input(PeerDown)；hub.set_status(connected=false)。
//! UDP：对方地址 = TCP 对端 IP + Hello 里的 udp_port；之后以最近一个认证通过的 UDP 包源地址为准。每秒发 UdpMsg::Hi。
//!   收包先过滤再解密：长度不在 25..=64、源 IP 不是当前会话 TCP 对端的 IP，直接丢。没有会话时丢弃所有 UDP。
//!   收到 Move → injector.moved。
//!   本机 UDP 不可用、还没收到过对方的 UDP 包、或 2.5 秒没收到（对方每秒发 Hi）：鼠标位置同时走 TCP（Msg::Move，
//!   排在已排队的消息之后、图片分片之前，只留最新的一个）；UDP 包再来就自动切回只走 UDP。
//!   最后一个 Move 在 100ms 内没有更新的就补发一次（同 seq，接收方去重），之后 3 秒内每秒再补一次。
//! 发送端 Out::Msg 在没有会话时直接丢弃（不排队），Out::ClipStream 同样丢弃（压缩线程随之停下）。
//! 已发出开始帧的流没发完就被丢弃（取消/被新内容顶掉/压缩线程放弃）：在同一会话里补发 FRAME_ABORT，对方立刻释放。
//!
//! 内存：没有常驻的大缓冲。读帧按帧长分配、用完即还；发送端只排着 ≤ 1 MiB 的压缩分片；UDP 接收缓冲 128 字节。

use crate::config::{self, ClipCfg, Config};
use crate::crypto::{self, AudioOpener, AudioSealer, Opener, Sealer, UdpOpener, UdpSealer};
use crate::geom::{Desk, Layout};
use crate::hub::{Hub, Out, Piece, StreamRx, ToDesktop, ToInput};
use crate::inject::Injector;
use crate::proto::{self, Frame, MAGIC, MAGIC_V1, Msg, PROTO_VERSION, UdpMsg};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 帧（密文）长度上限。
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// 认证前的第一帧上限（Hello 很小）。
const MAX_FIRST_FRAME: usize = 64 * 1024;
/// 明文上限（密文 = 明文 + 16 字节 tag）。
const MAX_PLAIN: usize = MAX_FRAME - 16;
/// 握手 + 第一帧的绝对期限。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(6);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PING_EVERY: Duration = Duration::from_secs(1);
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
const TIE_WINDOW: Duration = Duration::from_secs(2);
/// 旧会话这么久内收到过帧 = 仍活跃（另一台同密钥的电脑连进来时不替换它）。
const ALIVE_WINDOW: Duration = Duration::from_secs(2);
/// 这么久没收到对方任何帧（对方每秒 Ping）：抬起本机按着的键和按钮（会话不断）。
const STALE_AFTER: Duration = Duration::from_millis(1500);
/// 这么久没收到对方任何帧：本机若正在控制对方，先回到本机（会话不断；断线要等 READ_TIMEOUT）。
/// WiFi 的正常停顿在几百毫秒以内，2.5 秒足够不误判，又不让用户对着“失灵”的鼠标干等 6 秒。
const GOHOME_AFTER: Duration = Duration::from_millis(2500);
/// 鼠标正在两台之间移动时，图片分片的发送速率上限（字节/秒）。WiFi 实际吞吐约 15~20 MB/s，4 MB/s 只占一小部分，
/// 路由器/网卡队列不会积压；一般截图压缩后 1~3 MB，限速下也只要不到 1 秒。鼠标不动时全速。
const PACE_BPS: f64 = 4.0 * 1024.0 * 1024.0;
/// 最近这么久内有鼠标移动经过本机，就算“鼠标正在移动”。
const MOUSE_ACTIVE_MS: u64 = 500;

/// 进程内单调毫秒（从第一次调用起算，+1 保证不为 0）。
fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}
/// 同时进行中的入站握手上限（防止局域网里有东西乱连把线程堆满）。
const MAX_PENDING_HANDSHAKES: usize = 8;
/// 同一源 IP 同时进行中的入站握手上限。
const MAX_PENDING_PER_IP: u8 = 2;
/// 同一 IP 的同类错误日志至多每分钟一次。
const IP_LOG_EVERY: Duration = Duration::from_secs(60);
/// 最后一个 Move 之后多久补发一次。
const REPAIR_AFTER: Duration = Duration::from_millis(100);
/// 最后一个 Move 之后多久内还补发。
const REPAIR_FOR: Duration = Duration::from_secs(3);
/// 两次发鼠标位置的最小间隔（1000Hz/8000Hz 鼠标不至于每个事件一个包挤占 WiFi）。
const MOVE_GAP: Duration = Duration::from_millis(2);
/// 会话建立后这么久还没收到对方 UDP 包：报 UDP 不通。
const UDP_GRACE: Duration = Duration::from_secs(3);
const UDP_RETRY: Duration = Duration::from_secs(5);
/// 这么久没收到认证通过的 UDP 包（对方每秒发 Hi）：鼠标位置回落到 TCP。
const UDP_STALE: Duration = Duration::from_millis(2500);
/// 合法 UDP 包长度：8 字节计数器 + 明文（Hi 1 字节 / Move 21 字节，旧版 17）+ 16 字节 tag。
const UDP_MIN_PKT: usize = 8 + 1 + 16;
const UDP_MAX_PKT: usize = 64;
/// 声音包（计数器最高位为 1）：8 字节计数器 + 明文（至少 10 字节头）+ 16 字节 tag；上限按 IPv6 最小 MTU 留余量。
const AUDIO_MIN_PKT: usize = 8 + 10 + 16;
const AUDIO_MAX_PKT: usize = 1232;
/// UDP 接收缓冲：比合法包大就行（更大的包 recv_from 报 WSAEMSGSIZE，按单包错误跳过）。
const UDP_BUF: usize = 2048;
/// UDP 套接字的内核接收缓冲（WiFi 停顿后一下子涌来的包不至于被丢）。
const UDP_RCVBUF: i32 = 256 * 1024;
/// 发现连到自己后多久内不再拨同一个地址。
const SELF_BACKOFF: Duration = Duration::from_secs(60);
/// 记住本机作为客户端发出的 nonce_c 多久 / 最多几个。
const SENT_NC_KEEP: Duration = Duration::from_secs(10);
const SENT_NC_MAX: usize = 16;
/// 半截的接收传输这么久没进展就释放。
const CLIP_STALL: Duration = Duration::from_secs(30);
/// peer.json / config.toml 每秒至多写一次。
const PERSIST_EVERY: Duration = Duration::from_secs(1);
/// 对方排布的 rev 至多比本机时间超前这么多（毫秒）。
const MAX_REV_AHEAD: u64 = 24 * 3600 * 1000;
const DECRYPT_FAIL: &str = "解密失败";
const ERR_REFLECT: &str = "收到本机自己发出的握手";
const ERR_PEER_KEY: &str = "对方的临时公钥无效";
pub const ERR_SELF: &str = "对方地址指向本机";
pub const ERR_OLD: &str = "对方版本太旧，请两台都更新";
pub const ERR_UDP_DOWN: &str = "UDP 不可用，已改走 TCP";

pub fn spawn(hub: Arc<Hub>, rx: Receiver<Out>, injector: Arc<Injector>, audio: Arc<crate::audio::Port>) {
    let opts = Opts {
        bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        persist: true,
        dial_every: Duration::from_secs(2),
        id: proto::instance_id(),
        audio: Some(audio),
    };
    start(hub, rx, injector, opts);
}

struct Opts {
    bind_ip: IpAddr,
    /// 是否写盘（config.toml / peer.json）。测试里关掉。
    persist: bool,
    dial_every: Duration,
    /// Hello 里的 instance id（正式运行 = proto::instance_id()；测试里每个节点不同）。
    id: u64,
    /// 声音（测试里没有）。
    audio: Option<Arc<crate::audio::Port>>,
}

type SessionHandle = Arc<Session>;

struct Net {
    hub: Arc<Hub>,
    injector: Arc<Injector>,
    opts: Opts,
    /// 当前会话（至多一个）。
    holder: Mutex<Option<SessionHandle>>,
    /// 绑定成功后才有（失败时 net-udp 线程每 5 秒重试）。
    udp: OnceLock<UdpSocket>,
    next_id: AtomicU64,
    /// 认证前的入站连接（每 IP / 总数）。
    admit: Mutex<Admission>,
    key_warned: AtomicBool,
    /// 最近一次记下的错误（相同错误不重复写日志）。
    last_err: Mutex<String>,
    /// 发现“连到了自己”时的 (cfg.peer, 到期时间)：到期前 cfg.peer 不变就不拨。
    self_peer: Mutex<Option<(String, Instant)>>,
    /// 本机作为客户端最近发出的 nonce_c（listener 收到相同的 = 连到了自己 / 被反射）。
    sent_nc: Mutex<SentNonces>,
    /// dialer 最近一次解析出的 cfg.peer 的 IP。
    peer_ips: Mutex<Vec<IpAddr>>,
    /// 上次成功建立会话的对方 IP。
    last_ok: Mutex<Option<IpAddr>>,
    /// (IP, 类别) → 上次写日志的时间。
    ip_log: Mutex<HashMap<(IpAddr, &'static str), Instant>>,
    /// 可靠通道的发送队列：net-out 放，net-tcp 取。
    tq: Mutex<OutQueue>,
    tq_cv: Condvar,
    /// 每次 Out::CancelClip 加一：reader 看到变化就丢掉正在接收的对方图片。
    clip_gen: AtomicU64,
    /// 最近一次有鼠标移动经过本机（发出或收到）的时间（mono_ms）：这时图片限速发，别把 WiFi 占满让移动排队。
    mouse_at: AtomicU64,
    #[cfg(test)]
    stats: TestStats,
}

#[cfg(test)]
#[derive(Default)]
struct TestStats {
    tcp_moves: std::sync::atomic::AtomicU32,
    udp_moves: std::sync::atomic::AtomicU32,
    reflect_rejects: std::sync::atomic::AtomicU32,
    stales: std::sync::atomic::AtomicU32,
}

/// 会话建立时用的设置：变了就断开重连。
#[derive(Clone, PartialEq, Eq, Debug)]
struct CfgSig {
    key: Option<[u8; 32]>,
    peer: String,
    port: u16,
}

impl CfgSig {
    fn of(cfg: &Config) -> CfgSig {
        CfgSig { key: cfg.key_bytes(), peer: cfg.peer.trim().to_string(), port: cfg.port }
    }
}

struct Session {
    id: u64,
    started: Instant,
    nonce_c: [u8; 32],
    peer: SocketAddr,
    /// 对方的 instance id（旧版本为 0）。
    peer_id: u64,
    sig: CfgSig,
    /// 最近一次收到帧的时间。
    last_rx: Mutex<RxClock>,
    /// 本会话已记过“UDP 不通”的日志。
    udp_warned: AtomicBool,
    /// 只用来 shutdown。
    ctl: TcpStream,
    tx: Mutex<TcpTx>,
    udp_tx: Mutex<UdpSealer>,
    udp_rx: Mutex<UdpOpener>,
    udp_peer: Mutex<UdpPeer>,
    /// 声音：audio-cap 线程加密发送 / net-udp 解密（各自只有一个线程用，锁不争用）。
    audio_tx: Mutex<AudioSealer>,
    audio_rx: Mutex<AudioOpener>,
}

struct RxClock {
    at: Instant,
    /// 这段安静已经抬起过按键（有新帧来才重新计时）。
    staled: bool,
    /// 这一段安静期里已经让 input 回过本机。
    homed: bool,
}

struct TcpTx {
    stream: TcpStream,
    sealer: Sealer,
}

#[derive(Default)]
struct UdpPeer {
    addr: Option<SocketAddr>,
    /// UDP 通着：最近 2.5 秒内收到过认证通过的包（此时 Hello 不再覆盖 addr，鼠标位置只走 UDP）。
    learned: bool,
    /// 最近一次认证通过的 UDP 包的时间。
    last: Option<Instant>,
}

impl UdpPeer {
    /// 收到一个认证通过的 UDP 包。
    fn heard(&mut self, src: SocketAddr, now: Instant) {
        self.addr = Some(src);
        self.learned = true;
        self.last = Some(now);
    }

    /// UDP 现在还算通吗？2.5 秒没收到就退回“不通”（鼠标改走 TCP），再收到包时 heard 会切回来。
    fn live(&mut self, now: Instant) -> bool {
        if self.learned && self.last.is_none_or(|t| now.saturating_duration_since(t) > UDP_STALE) {
            self.learned = false;
        }
        self.learned
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 起线程；失败返回 false（闭包已丢弃）。
fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(_) => true,
        Err(e) => {
            crate::log!("net：创建线程 {name} 失败：{e}");
            false
        }
    }
}

fn bind_udp(ip: IpAddr, port: u16) -> io::Result<UdpSocket> {
    let s = UdpSocket::bind((ip, port))?;
    set_rcvbuf(&s, UDP_RCVBUF);
    Ok(s)
}

/// 调大 UDP 接收缓冲。失败无所谓（用系统默认）。
fn set_rcvbuf(s: &UdpSocket, bytes: i32) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_RCVBUF, SOCKET, SOL_SOCKET, setsockopt};
    let v = bytes.to_le_bytes();
    // SAFETY：s 存活期间套接字句柄有效；optval 指向 4 字节的 int
    unsafe {
        let _ = setsockopt(SOCKET(s.as_raw_socket() as usize), SOL_SOCKET, SO_RCVBUF, Some(&v));
    }
}

/// TCP 发送缓冲上限。Windows 默认会把发送缓冲自动调到几百 KB 甚至几 MB：传图时内核里积压一大堆分片，
/// 之后的 Enter / 按键 / 点击都排在它们后面（实测传 14MB 图时跨屏要等约 90ms）。限成 64 KiB：
/// 排在前面的至多 64 KiB（WiFi 上约 10ms）；局域网往返只有几毫秒，64 KiB 的窗口传图仍有十几 MB/s。
const TCP_SNDBUF: i32 = 64 * 1024;

fn set_sndbuf(s: &TcpStream, bytes: i32) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_SNDBUF, SOCKET, SOL_SOCKET, setsockopt};
    let v = bytes.to_le_bytes();
    // SAFETY：s 存活期间套接字句柄有效；optval 指向 4 字节的 int
    unsafe {
        let _ = setsockopt(SOCKET(s.as_raw_socket() as usize), SOL_SOCKET, SO_SNDBUF, Some(&v));
    }
}

fn start(hub: Arc<Hub>, rx: Receiver<Out>, injector: Arc<Injector>, opts: Opts) -> Arc<Net> {
    let port = hub.cfg().port;
    let udp = OnceLock::new();
    match bind_udp(opts.bind_ip, port) {
        Ok(s) => {
            let _ = udp.set(s);
        }
        Err(e) => {
            crate::log!("UDP 端口 {port} 绑定失败：{e}（鼠标移动改走 TCP，每 5 秒重试）");
            hub.set_status(|s| s.last_error = ERR_UDP_DOWN.into());
        }
    }
    let net = Arc::new(Net {
        hub,
        injector,
        opts,
        holder: Mutex::new(None),
        udp,
        next_id: AtomicU64::new(1),
        admit: Mutex::new(Admission::default()),
        key_warned: AtomicBool::new(false),
        last_err: Mutex::new(String::new()),
        self_peer: Mutex::new(None),
        sent_nc: Mutex::new(SentNonces::default()),
        peer_ips: Mutex::new(Vec::new()),
        last_ok: Mutex::new(None),
        ip_log: Mutex::new(HashMap::new()),
        tq: Mutex::new(OutQueue::default()),
        tq_cv: Condvar::new(),
        clip_gen: AtomicU64::new(0),
        mouse_at: AtomicU64::new(0),
        #[cfg(test)]
        stats: TestStats::default(),
    });
    let n = net.clone();
    spawn_named("net-out", move || n.run_out(rx));
    let n = net.clone();
    spawn_named("net-tcp", move || n.run_tcp());
    let n = net.clone();
    spawn_named("net-listener", move || n.run_listener());
    let n = net.clone();
    spawn_named("net-dialer", move || n.run_dialer());
    let n = net.clone();
    spawn_named("net-udp", move || n.run_udp(port));
    if let Some(a) = &net.opts.audio {
        let w = Arc::downgrade(&net);
        a.set_sender(Box::new(move |sid, plain| w.upgrade().is_some_and(|n| n.send_audio(sid, plain))));
    }
    net
}

// ---------------------------------------------------------------- 握手与分帧

struct Chans {
    nonce_c: [u8; 32],
    tx: Sealer,
    rx: Opener,
    utx: UdpSealer,
    urx: UdpOpener,
    atx: AudioSealer,
    arx: AudioOpener,
}

/// 握手期的绝对截止时间：每次读写前把超时设为剩余时间（防止慢速连接一直占着握手名额）。
struct Deadline<'a> {
    s: &'a mut TcpStream,
    until: Instant,
}

impl Deadline<'_> {
    fn left(&self) -> io::Result<Duration> {
        let d = self.until.saturating_duration_since(Instant::now());
        if d.is_zero() { Err(io::Error::new(io::ErrorKind::TimedOut, "握手超时")) } else { Ok(d) }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let d = self.left()?;
        self.s.set_read_timeout(Some(d))?;
        self.s.read(buf)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let d = self.left()?;
        self.s.set_write_timeout(Some(d))?;
        self.s.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// 带固定说明的 InvalidData 错误（用 is_err 识别）。
fn err(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn is_err(e: &io::Error, msg: &str) -> bool {
    e.kind() == io::ErrorKind::InvalidData && e.to_string() == msg
}

/// 对方断开了连接（EOF / RST）。
fn is_closed(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted)
}

/// 读对方的握手：MAGIC + nonce + 临时公钥。
fn read_hs<S: Read>(s: &mut S) -> io::Result<([u8; 32], [u8; 32])> {
    let mut m = [0u8; 4];
    s.read_exact(&mut m)?;
    if &m == MAGIC_V1 {
        return Err(err(ERR_OLD));
    }
    if &m != MAGIC {
        return Err(err("握手魔数不对"));
    }
    let mut b = [0u8; 64];
    s.read_exact(&mut b)?;
    let (mut n, mut pk) = ([0u8; 32], [0u8; 32]);
    n.copy_from_slice(&b[..32]);
    pk.copy_from_slice(&b[32..]);
    Ok((n, pk))
}

/// 握手里本端的角色。
enum Role<'a> {
    /// 客户端：带上自己的 nonce_c（发之前已登记，见 SentNonces）。
    Client([u8; 32]),
    /// 服务端：判断收到的 nonce_c 是不是本机刚作为客户端发出去的。
    Server(&'a dyn Fn(&[u8; 32]) -> bool),
}

/// 明文握手 + 派生密钥。client 先发。
fn handshake<S: Read + Write>(s: &mut S, psk: &[u8; 32], role: Role) -> io::Result<Chans> {
    let eph = crypto::Ephemeral::new();
    let client = matches!(role, Role::Client(_));
    let mine = match role {
        Role::Client(n) => n,
        Role::Server(_) => crypto::random32(),
    };
    let mut hello = [0u8; 68];
    hello[..4].copy_from_slice(MAGIC);
    hello[4..36].copy_from_slice(&mine);
    hello[36..].copy_from_slice(&eph.pk);
    let (nc, ns, their_pk) = match role {
        Role::Client(_) => {
            s.write_all(&hello)?;
            let (ns, pk) = read_hs(s)?;
            (mine, ns, pk)
        }
        Role::Server(seen) => {
            let (nc, pk) = read_hs(s)?;
            if seen(&nc) {
                return Err(err(ERR_REFLECT));
            }
            s.write_all(&hello)?;
            (nc, mine, pk)
        }
    };
    let mut dh = eph.agree(&their_pk).ok_or_else(|| err(ERR_PEER_KEY))?;
    let k = crypto::derive(psk, &dh, &nc, &ns);
    crypto::wipe(&mut dh);
    Ok(if client {
        Chans {
            nonce_c: nc,
            tx: Sealer::new(&k.c2s),
            rx: Opener::new(&k.s2c),
            utx: UdpSealer::new(&k.udp_c2s),
            urx: UdpOpener::new(&k.udp_s2c),
            atx: AudioSealer::new(&k.audio_c2s),
            arx: AudioOpener::new(&k.audio_s2c),
        }
    } else {
        Chans {
            nonce_c: nc,
            tx: Sealer::new(&k.s2c),
            rx: Opener::new(&k.c2s),
            utx: UdpSealer::new(&k.udp_s2c),
            urx: UdpOpener::new(&k.udp_c2s),
            atx: AudioSealer::new(&k.audio_s2c),
            arx: AudioOpener::new(&k.audio_c2s),
        }
    })
}

fn write_frame<W: Write>(w: &mut W, sealer: &mut Sealer, plain: &[u8]) -> io::Result<()> {
    if plain.len() > MAX_PLAIN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "帧太大"));
    }
    // 计数器用尽（实际不可能）：返回非 InvalidInput 的错误，会话随之断开
    let ct = sealer.seal(plain).ok_or_else(|| io::Error::other("发送计数器用尽"))?;
    let mut buf = Vec::with_capacity(4 + ct.len());
    buf.extend_from_slice(&(ct.len() as u32).to_le_bytes());
    buf.extend_from_slice(&ct);
    w.write_all(&buf)
}

/// 读一帧（密文 ≤ max）并解密。解不开（篡改/重放/密钥不同/计数器用尽）返回 InvalidData + DECRYPT_FAIL。
fn read_frame<R: Read>(r: &mut R, opener: &mut Opener, max: usize) -> io::Result<Vec<u8>> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb)?;
    let len = u32::from_le_bytes(lb) as usize;
    if !(16..=max).contains(&len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("帧长度不合法：{len}")));
    }
    let mut ct = vec![0u8; len];
    r.read_exact(&mut ct)?;
    opener.open(&ct).ok_or_else(|| err(DECRYPT_FAIL))
}

fn is_auth_fail(e: &io::Error) -> bool {
    is_err(e, DECRYPT_FAIL)
}

/// 新会话是否应让位给旧会话（tie-break）。
fn keep_old(old_age: Duration, old_nonce_c: &[u8; 32], new_nonce_c: &[u8; 32]) -> bool {
    old_age < TIE_WINDOW && old_nonce_c > new_nonce_c
}

/// 旧会话连着另一台（id 不同）且仍活跃：拒绝新来的。id 为 0（旧版本）不判断。
fn other_peer_busy(old_id: u64, new_id: u64, old_idle: Duration) -> bool {
    old_id != 0 && new_id != 0 && old_id != new_id && old_idle < ALIVE_WINDOW
}

/// 对方的排布（已镜像成本机视角）是否采用：rev 更新；或 rev 相同、排布不同且本机 id 较小。
fn adopt_layout(cur_rev: u64, cur: &Layout, theirs: &Layout, rev: u64, my_id: u64, peer_id: u64) -> bool {
    rev > cur_rev || (rev == cur_rev && cur != theirs && peer_id != 0 && my_id < peer_id)
}

/// 对方发来的排布是否像样：rev 不明显超前（> 本机时间 + 1 天就不要，免得它永远压住本机的修改）、数值在合理范围。
fn layout_sane(l: &Layout, rev: u64, now_ms: u64) -> bool {
    rev <= now_ms.saturating_add(MAX_REV_AHEAD)
        && l.offset.unsigned_abs() <= proto::MAX_COORD as u32
        && l.scale.is_finite()
        && l.scale > 0.0
        && l.scale < 1000.0
}

/// 发现自连时记下的 (peer, 到期时间)：到期前配置的 peer 还是它就不拨；变了或到期就清掉。
fn self_blocked(slot: &mut Option<(String, Instant)>, peer: &str, now: Instant) -> bool {
    match slot {
        Some((p, until)) if p == peer && now < *until => true,
        Some(_) => {
            *slot = None;
            false
        }
        None => false,
    }
}

/// 认证前的错误（密钥不一致/版本太旧）要不要显示在状态里：已有会话时不改；
/// 填了对方地址时只认对方地址 / 上次连上的地址 —— 否则没有密钥的人随便连一下就能把状态改掉。
fn show_notice(has_session: bool, no_peer_cfg: bool, known_ip: bool) -> bool {
    !has_session && (no_peer_cfg || known_ip)
}

/// cfg.peer 若是 IP 字面量（可带端口），取出 IP（不做 DNS）。
fn peer_literal_ip(peer: &str) -> Option<IpAddr> {
    let p = peer.trim();
    if let Ok(sa) = p.parse::<SocketAddr>() {
        return Some(sa.ip());
    }
    p.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(p).parse().ok()
}

/// 鼠标位置走哪条路：(UDP, TCP)。UDP 可用就发 UDP；UDP 不通（还没收到过 / 2.5 秒没收到对方 UDP 包）时同时走 TCP。
fn move_route(udp_bound: bool, udp_live: bool) -> (bool, bool) {
    (udp_bound, !(udp_bound && udp_live))
}

/// 剪贴板分片传输的上限；None = 不接收。format 13 是大段文字（UTF-16LE）。
fn clip_limit(c: &ClipCfg, format: u32) -> Option<usize> {
    if !c.enabled {
        None
    } else if format == crate::clip::CF_UNICODETEXT {
        Some((c.max_text_kb as usize * 1024 + 1) * 2)
    } else if c.images {
        Some(c.max_image_mb as usize * 1024 * 1024)
    } else {
        None
    }
}

/// UDP 收包错误是否只关乎这一个包（不用退避）。
fn per_datagram_err(e: &io::Error) -> bool {
    // 10040 WSAEMSGSIZE：包比缓冲区大；10054：之前发往关闭端口的包
    matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted) || matches!(e.raw_os_error(), Some(10040 | 10054))
}

/// 解析对方地址：先当 IP 字面量（裸 IPv6、[IPv6]、带端口），否则当主机名[:端口]。
fn resolve_peer(peer: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let p = peer.trim();
    if let Ok(sa) = p.parse::<SocketAddr>() {
        return Ok(vec![sa]);
    }
    let bare = p.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(p);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let (host, port) = match p.rsplit_once(':') {
        Some((h, ps)) if !h.contains(':') => {
            (h, ps.parse::<u16>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "端口不对"))?)
        }
        _ => (p, port),
    };
    Ok((host, port).to_socket_addrs()?.collect())
}

// ---------------------------------------------------------------- 认证前的防护（纯逻辑）

/// 认证前的入站连接计数：每个源 IP 至多 MAX_PENDING_PER_IP 个；总数至多 MAX_PENDING_HANDSHAKES 个，
/// 但 reserved（cfg.peer 的 IP、上次连上的 IP）总数满了也放进来 —— 别人乱连挤不掉真正的对方。
#[derive(Default)]
struct Admission {
    per_ip: HashMap<IpAddr, u8>,
    total: usize,
}

impl Admission {
    fn admit(&mut self, ip: IpAddr, reserved: bool) -> bool {
        let n = self.per_ip.get(&ip).copied().unwrap_or(0);
        if n >= MAX_PENDING_PER_IP || (self.total >= MAX_PENDING_HANDSHAKES && !reserved) {
            return false;
        }
        self.per_ip.insert(ip, n + 1);
        self.total += 1;
        true
    }

    fn release(&mut self, ip: IpAddr) {
        if let Some(n) = self.per_ip.get_mut(&ip) {
            *n -= 1;
            if *n == 0 {
                self.per_ip.remove(&ip);
            }
            self.total -= 1;
        }
    }
}

struct SentNc {
    nc: [u8; 32],
    at: Instant,
    /// listener 收到过它（= 连到了自己 / 被反射）。
    hit: bool,
}

/// 本机作为客户端最近 10 秒发出的 nonce_c（至多 16 个）。
#[derive(Default)]
struct SentNonces(VecDeque<SentNc>);

impl SentNonces {
    fn remember(&mut self, nc: [u8; 32], now: Instant) {
        self.0.retain(|x| now.saturating_duration_since(x.at) < SENT_NC_KEEP);
        if self.0.len() >= SENT_NC_MAX {
            self.0.pop_front();
        }
        self.0.push_back(SentNc { nc, at: now, hit: false });
    }

    /// listener 用：这个 nonce_c 是本机最近发出去的吗？是就记下（dialer 据此知道“连到了自己”）。
    fn reflected(&mut self, nc: &[u8; 32], now: Instant) -> bool {
        match self.0.iter_mut().find(|x| x.nc == *nc && now.saturating_duration_since(x.at) < SENT_NC_KEEP) {
            Some(x) => {
                x.hit = true;
                true
            }
            None => false,
        }
    }

    /// dialer 用：自己发出的这个 nonce_c 被本机 listener 收到过吗？
    fn hit(&self, nc: &[u8; 32]) -> bool {
        self.0.iter().any(|x| x.nc == *nc && x.hit)
    }
}

// ---------------------------------------------------------------- 鼠标位置（纯逻辑）

/// 最近发出的 Move（用于补发）。
struct LastMove {
    sid: u64,
    mv: UdpMsg,
    at: Instant,
    repaired: bool,
}

impl LastMove {
    /// 现在是否该补发：3 秒内，100ms 没有新的就补一次；每秒的 tick 再补。
    fn due(&self, now: Instant, tick: bool) -> bool {
        let age = now.saturating_duration_since(self.at);
        age < REPAIR_FOR && ((!self.repaired && age >= REPAIR_AFTER) || tick)
    }
}

/// 鼠标位置的发送节奏：只保留最新的一个；两次发送至少隔 MOVE_GAP，间隔一到立刻发最新值；之后按 LastMove 补发。
#[derive(Default)]
struct Mover {
    pending: Option<UdpMsg>,
    sent: Option<Instant>,
    last: Option<LastMove>,
}

impl Mover {
    fn push(&mut self, m: UdpMsg) {
        self.pending = Some(m);
    }

    /// 下次需要醒来的时间（没有要做的返回 None）。
    fn wake(&self) -> Option<Instant> {
        if self.pending.is_some() {
            return Some(self.sent.map_or_else(Instant::now, |t| t + MOVE_GAP));
        }
        self.last.as_ref().filter(|l| !l.repaired).map(|l| l.at + REPAIR_AFTER)
    }

    /// 现在该发的那个（新位置或补发）。sid = 当前会话。
    fn poll(&mut self, now: Instant, tick: bool, sid: u64) -> Option<UdpMsg> {
        if self.last.as_ref().is_some_and(|l| l.sid != sid || now.saturating_duration_since(l.at) >= REPAIR_FOR) {
            self.last = None;
        }
        if let Some(m) = self.pending {
            if self.sent.is_some_and(|t| now < t + MOVE_GAP) {
                return None;
            }
            self.pending = None;
            self.sent = Some(now);
            self.last = Some(LastMove { sid, mv: m, at: now, repaired: false });
            return Some(m);
        }
        let l = self.last.as_mut()?;
        if l.due(now, tick) {
            l.repaired = true;
            self.sent = Some(now);
            return Some(l.mv);
        }
        None
    }
}

// ---------------------------------------------------------------- 会话

impl Session {
    fn send_plain(&self, plain: &[u8]) -> io::Result<()> {
        let mut tx = lock(&self.tx);
        let TcpTx { stream, sealer } = &mut *tx;
        let r = write_frame(stream, sealer, plain);
        if let Err(e) = &r {
            if e.kind() != io::ErrorKind::InvalidInput {
                self.close();
            }
        }
        r
    }
    fn close(&self) {
        let _ = self.ctl.shutdown(Shutdown::Both);
    }
    fn send_udp(&self, sock: &UdpSocket, m: &UdpMsg) {
        let Some(addr) = lock(&self.udp_peer).addr else { return };
        let Some(pkt) = lock(&self.udp_tx).seal(&m.encode()) else { return };
        let _ = sock.send_to(&pkt, addr);
    }
    fn udp_live(&self) -> bool {
        lock(&self.udp_peer).live(Instant::now())
    }
}

fn layout_msg(cfg: &Config) -> Msg {
    let l = cfg.layout.layout;
    Msg::Layout { side: l.side, offset: l.offset, scale: l.scale, rev: cfg.layout.rev }
}

/// reader 线程自己的状态。
#[derive(Default)]
struct RxState {
    clip: ClipRx,
    /// 已处理到的 Net::clip_gen。
    clip_seen: u64,
    /// 等着写盘的 peer.json / 排布，及上次写的时间（每秒至多一次）。
    desk: Option<(String, Desk, Vec<u32>)>,
    desk_at: Option<Instant>,
    layout: Option<(Layout, u64)>,
    layout_at: Option<Instant>,
}

impl Net {
    fn current(&self) -> Option<SessionHandle> {
        lock(&self.holder).clone()
    }

    fn is_current(&self, s: &Session) -> bool {
        lock(&self.holder).as_ref().is_some_and(|c| c.id == s.id)
    }

    /// 记录错误：相同的错误只写一次日志；同时显示在状态里。
    fn note_err(&self, msg: String) {
        {
            let mut last = lock(&self.last_err);
            if *last == msg {
                return;
            }
            last.clone_from(&msg);
        }
        crate::log!("{msg}");
        self.hub.set_status(|s| s.last_error = msg);
    }

    fn clear_err(&self) {
        lock(&self.last_err).clear();
    }

    /// 按 (IP, 类别) 限频的日志（每分钟一次）；status = 同时显示在状态里。
    fn log_ip(&self, ip: IpAddr, kind: &'static str, msg: String, status: bool) {
        {
            let mut m = lock(&self.ip_log);
            let now = Instant::now();
            if m.get(&(ip, kind)).is_some_and(|t| now.duration_since(*t) < IP_LOG_EVERY) {
                return;
            }
            if m.len() >= 256 {
                m.retain(|_, t| now.duration_since(*t) < IP_LOG_EVERY);
            }
            m.insert((ip, kind), now);
        }
        crate::log!("{msg}");
        if status {
            self.hub.set_status(|s| s.last_error = msg);
        }
    }

    /// 是 cfg.peer 的 IP 或上次连上的 IP（预留握手名额、可以改状态）。不做 DNS：用 dialer 解析好的。
    fn known_ip(&self, ip: IpAddr, peer_cfg: &str) -> bool {
        peer_literal_ip(peer_cfg) == Some(ip) || lock(&self.peer_ips).contains(&ip) || *lock(&self.last_ok) == Some(ip)
    }

    /// 认证前的错误：总记日志（限频），按 show_notice 决定是否显示在状态里。
    fn peer_notice(&self, ip: IpAddr, kind: &'static str, msg: String) {
        let peer_cfg = self.hub.cfg.read().unwrap_or_else(|e| e.into_inner()).peer.trim().to_string();
        let show = show_notice(self.current().is_some(), peer_cfg.is_empty(), self.known_ip(ip, &peer_cfg));
        self.log_ip(ip, kind, msg, show);
    }

    /// 取密钥；无效时只记一次日志。
    fn psk(&self, cfg: &Config) -> Option<[u8; 32]> {
        match cfg.key_bytes() {
            Some(k) => {
                self.key_warned.store(false, Ordering::Relaxed);
                Some(k)
            }
            None => {
                if !self.key_warned.swap(true, Ordering::Relaxed) {
                    crate::log!("配置里的密钥无效（需要 64 个十六进制字符），不连接");
                    self.hub.set_status(|s| s.last_error = "密钥无效".into());
                }
                None
            }
        }
    }

    fn hello(&self, cfg: &Config) -> Msg {
        let udp_port = self.udp.get().and_then(|u| u.local_addr().ok()).map(|a| a.port()).unwrap_or(cfg.port);
        let (desk, dpis) = crate::sys::monitors_dpi();
        Msg::Hello { ver: PROTO_VERSION, name: cfg.name.clone(), monitors: desk.monitors, udp_port, id: self.opts.id, dpis, caps: proto::CAPS }
    }

    fn send_hello<W: Write>(&self, w: &mut W, tx: &mut Sealer, cfg: &Config) -> io::Result<()> {
        write_frame(w, tx, &proto::encode_msg(&self.hello(cfg)))?;
        write_frame(w, tx, &proto::encode_msg(&layout_msg(cfg)))
    }

    /// 发现连到了自己：报错，60 秒内不再拨这个地址。
    fn self_detected(&self, peer: &str) {
        *lock(&self.self_peer) = Some((peer.to_string(), Instant::now() + SELF_BACKOFF));
        self.note_err(ERR_SELF.into());
    }

    /// 握手失败的报告。dialed：主动连接时的 cfg.peer；my_nc：本机作为客户端发出的 nonce_c。
    fn handshake_failed(&self, e: &io::Error, peer: SocketAddr, dialed: Option<&str>, my_nc: &[u8; 32]) {
        let ip = peer.ip();
        match dialed {
            Some(p) => {
                if lock(&self.sent_nc).hit(my_nc) {
                    self.self_detected(p);
                } else if is_err(e, ERR_OLD) {
                    self.note_err(ERR_OLD.into());
                } else if is_closed(e) {
                    self.note_err(format!("与 {peer} 握手失败：对方断开了连接（若对方是旧版本，请两台都更新）"));
                } else {
                    self.note_err(format!("与 {peer} 握手失败：{e}"));
                }
            }
            None => {
                if is_err(e, ERR_REFLECT) {
                    #[cfg(test)]
                    self.stats.reflect_rejects.fetch_add(1, Ordering::Relaxed);
                    self.log_ip(ip, "self", format!("{ip} 发来的握手是本机刚发出的（连到了自己），拒绝"), false);
                } else if is_err(e, ERR_OLD) {
                    self.peer_notice(ip, "old", ERR_OLD.into());
                } else {
                    self.log_ip(ip, "hs", format!("{ip} 入站握手失败：{e}"), false);
                }
            }
        }
    }

    /// 握手、读对方 Hello（= 认证）、互发 Hello/Layout、登记为当前会话。
    /// 客户端先发 Hello；服务端验证了客户端的第一帧才发自己的（认证前不发可预测的内容）。
    /// dialed：主动连接时配置里的 peer 字符串（被动连接为 None）。
    fn establish(&self, mut stream: TcpStream, dialed: Option<&str>) -> Option<(SessionHandle, TcpStream, Opener, Msg)> {
        let client = dialed.is_some();
        let cfg = self.hub.cfg();
        let psk = self.psk(&cfg)?;
        let peer = stream.peer_addr().ok()?;
        let ip = peer.ip();
        let _ = stream.set_nodelay(true);
        set_sndbuf(&stream, TCP_SNDBUF);

        let mut dl = Deadline { s: &mut stream, until: Instant::now() + HANDSHAKE_TIMEOUT };
        let my_nc = crypto::random32();
        let seen = |nc: &[u8; 32]| lock(&self.sent_nc).reflected(nc, Instant::now());
        let role = if client {
            lock(&self.sent_nc).remember(my_nc, Instant::now());
            Role::Client(my_nc)
        } else {
            Role::Server(&seen)
        };
        let ch = match handshake(&mut dl, &psk, role) {
            Ok(c) => c,
            Err(e) => {
                self.handshake_failed(&e, peer, dialed, &my_nc);
                return None;
            }
        };
        let Chans { nonce_c, mut tx, mut rx, utx, urx, atx, arx } = ch;
        if client {
            if let Err(e) = self.send_hello(&mut dl, &mut tx, &cfg) {
                self.note_err(format!("向 {peer} 发送 Hello 失败：{e}"));
                return None;
            }
        }
        let first = match read_frame(&mut dl, &mut rx, MAX_FIRST_FRAME) {
            Ok(p) => p,
            Err(e) if is_auth_fail(&e) => {
                self.peer_notice(ip, "auth", format!("与 {ip} 认证失败：两边的配对密钥不一致"));
                return None;
            }
            Err(e) if client && is_closed(&e) => {
                // 服务端解不开我们的第一帧就会直接断开：多半是密钥不一致（客户端只连 cfg.peer，可以显示）
                self.log_ip(ip, "auth", format!("与 {ip} 认证失败：对方断开了连接（多半是两边的配对密钥不一致）"), true);
                return None;
            }
            Err(e) => {
                self.log_ip(ip, "hello", format!("等待 {ip} 的 Hello 失败：{e}"), false);
                return None;
            }
        };
        let hello = match proto::decode_frame(&first) {
            Some(Frame::Msg(m @ Msg::Hello { .. })) => m,
            _ => {
                self.log_ip(ip, "first", format!("{ip} 的第一帧不是 Hello，断开"), false);
                return None;
            }
        };
        let Msg::Hello { ver, udp_port, id: peer_id, .. } = &hello else { return None };
        if *ver != PROTO_VERSION {
            self.note_err(format!("对方协议版本 {ver} 与本机 {PROTO_VERSION} 不同，请两边升级到同一版本"));
            return None;
        }
        if *peer_id == self.opts.id {
            match dialed {
                Some(p) => self.self_detected(p),
                None => self.note_err(ERR_SELF.into()),
            }
            return None;
        }
        if !client {
            if let Err(e) = self.send_hello(&mut dl, &mut tx, &cfg) {
                self.log_ip(ip, "send", format!("向 {ip} 发送 Hello 失败：{e}"), false);
                return None;
            }
        }
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let ctl = stream.try_clone().ok()?;
        let wstream = stream.try_clone().ok()?;
        let sess = Arc::new(Session {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            nonce_c,
            peer,
            peer_id: *peer_id,
            sig: CfgSig::of(&cfg),
            last_rx: Mutex::new(RxClock { at: Instant::now(), staled: false, homed: false }),
            udp_warned: AtomicBool::new(false),
            ctl,
            tx: Mutex::new(TcpTx { stream: wstream, sealer: tx }),
            udp_tx: Mutex::new(utx),
            udp_rx: Mutex::new(urx),
            udp_peer: Mutex::new(UdpPeer { addr: Some(SocketAddr::new(ip, *udp_port)), ..Default::default() }),
            audio_tx: Mutex::new(atx),
            audio_rx: Mutex::new(arx),
        });
        if let Err(why) = self.install(&sess) {
            self.log_ip(ip, "yield", format!("与 {ip} 的新连接被拒：{why}"), false);
            sess.close();
            return None;
        }
        self.clear_err();
        crate::log!("已连接 {peer}（{}）", if client { "主动" } else { "被动" });
        Some((sess, stream, rx, hello))
    }

    /// 登记新会话；按 tie-break / 身份规则可能拒绝。
    /// injector.begin_session 和 PeerDown 在 holder 锁内发出：两条会话并发登记时，injector 的会话号与 holder 一致。
    fn install(&self, new: &SessionHandle) -> Result<(), String> {
        {
            let mut cur = lock(&self.holder);
            if let Some(old) = cur.as_ref() {
                if keep_old(old.started.elapsed(), &old.nonce_c, &new.nonce_c) {
                    return Err("让位给已有连接".into());
                }
                if other_peer_busy(old.peer_id, new.peer_id, lock(&old.last_rx).at.elapsed()) {
                    return Err(format!("已连着另一台（{}）", old.peer.ip()));
                }
                old.close();
            }
            if let (Some(a), Some(old)) = (&self.opts.audio, cur.as_ref()) {
                a.session_down(old.id);
            }
            let replaced = cur.replace(new.clone()).is_some();
            self.hub.to_desktop(ToDesktop::ClipForget);
            self.injector.begin_session(new.id);
            if replaced {
                crate::log!("新连接替换旧连接");
                self.hub.to_input(ToInput::PeerDown);
            }
        }
        *lock(&self.last_ok) = Some(new.peer.ip());
        *lock(&self.self_peer) = None;
        Ok(())
    }

    fn end_session(&self, sess: &Session, why: &str) {
        sess.close();
        let mut cur = lock(&self.holder);
        if !cur.as_ref().is_some_and(|c| c.id == sess.id) {
            return;
        }
        *cur = None;
        // 同 install：在锁内通知，和并发登记的新会话顺序一致
        crate::log!("与 {} 的连接断开：{why}", sess.peer);
        if let Some(a) = &self.opts.audio {
            a.session_down(sess.id);
        }
        self.injector.end_session(sess.id);
        self.hub.to_input(ToInput::PeerDown);
        self.hub.set_status(|s| {
            s.connected = false;
            s.peer_addr.clear();
        });
    }

    fn run_reader(&self, sess: SessionHandle, mut stream: TcpStream, mut opener: Opener, first: Msg) {
        // 按键/按钮在这里注入；它也解压剪贴板，所以用 HIGHEST 而不是最高档
        crate::sys::boost_thread(false);
        let mut st = RxState { clip_seen: self.clip_gen.load(Ordering::Relaxed), ..Default::default() };
        self.handle(&sess, first, &mut st);
        self.flush(&mut st, false);
        let why = loop {
            let plain = match read_frame(&mut stream, &mut opener, MAX_FRAME) {
                Ok(p) => p,
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                    break "超过 6 秒没有收到数据".to_string();
                }
                Err(e) => break e.to_string(),
            };
            if !self.is_current(&sess) {
                break "已被新连接替换".to_string();
            }
            {
                let mut c = lock(&sess.last_rx);
                c.at = Instant::now();
                c.staled = false;
                c.homed = false;
            }
            // 本机剪贴板已被更新的内容取代：丢掉正在接收的对方图片（之后这个 id 的分片都忽略）
            let g = self.clip_gen.load(Ordering::Relaxed);
            if g != st.clip_seen {
                st.clip_seen = g;
                st.clip.reset();
            }
            match proto::decode_frame(&plain) {
                Some(Frame::Msg(m)) => self.handle(&sess, m, &mut st),
                Some(Frame::Chunk { id, offset, data }) => st.clip.chunk(id, offset, data),
                Some(Frame::End { id, total }) => {
                    if let Some(b) = st.clip.end(id, total) {
                        crate::log!("收到对方的剪贴板数据（格式 {}）", b.format);
                        self.hub.to_desktop(ToDesktop::ClipData(b));
                    }
                }
                Some(Frame::Abort { id }) => st.clip.abort(id),
                None => crate::log!("忽略无法解析的帧（{} 字节）", plain.len()),
            }
            drop(plain);
            st.clip.expire(Instant::now());
            if std::mem::take(&mut st.clip.lost) {
                self.clip_lost();
            }
            self.flush(&mut st, false);
        };
        self.flush(&mut st, true);
        self.end_session(&sess, &why);
    }

    /// 处理一条消息。只在 reader 线程调用：绝不直接写 socket（回复走可靠队列 → net-tcp）。
    fn handle(&self, sess: &Session, m: Msg, st: &mut RxState) {
        match m {
            Msg::Hello { ver, name, monitors, udp_port, dpis, caps, .. } => {
                if ver != PROTO_VERSION {
                    crate::log!("对方协议版本 {ver} 与本机 {PROTO_VERSION} 不同");
                }
                let name = proto::sanitize_name(name);
                let n_in = monitors.len();
                let (monitors, dpis) = proto::sanitize_desk(monitors, dpis);
                if monitors.len() != n_in {
                    crate::log!("对方报来 {n_in} 块显示器，有效的 {} 块", monitors.len());
                }
                let desk = Desk::new(monitors);
                if self.opts.persist {
                    st.desk = Some((name.clone(), desk.clone(), dpis));
                }
                {
                    let mut u = lock(&sess.udp_peer);
                    if !u.learned {
                        u.addr = Some(SocketAddr::new(sess.peer.ip(), udp_port));
                    }
                }
                let addr = sess.peer.to_string();
                let n = name.clone();
                let udp_down = self.udp.get().is_none();
                self.hub.set_status(|s| {
                    s.connected = true;
                    s.peer_name = n;
                    s.peer_addr = addr;
                    if udp_down {
                        s.last_error = ERR_UDP_DOWN.into();
                    } else {
                        s.last_error.clear();
                    }
                });
                if let Some(a) = &self.opts.audio {
                    // 和 install / end_session 同一把锁：已被替换/已结束的会话迟到的 Hello 不会把声音挂到死会话上
                    let cur = lock(&self.holder);
                    if cur.as_ref().is_some_and(|c| c.id == sess.id) {
                        a.session_up(sess.id, caps, sess.peer_id, &name);
                    }
                }
                self.hub.to_input(ToInput::PeerUp { name, desk, caps });
            }
            Msg::Layout { side, offset, scale, rev } => self.apply_layout(sess, Layout { side, offset, scale }, rev, st),
            Msg::Enter { epoch, x, y, prev } => self.injector.enter_from(sess.id, epoch, x, y, prev),
            Msg::Leave { epoch } => self.injector.leave(sess.id, epoch),
            Msg::Button { epoch, x, y, button, down } => self.injector.button(sess.id, epoch, x, y, button, down),
            Msg::Wheel { epoch, x, y, delta, horizontal } => self.injector.wheel(sess.id, epoch, x, y, delta, horizontal),
            Msg::Key { epoch, scan, ext, vk, down } => self.injector.key(sess.id, epoch, scan, ext, vk, down),
            Msg::Move { epoch, seq, x, y } => {
                self.mark_mouse();
                #[cfg(test)]
                self.stats.tcp_moves.fetch_add(1, Ordering::Relaxed);
                self.injector.moved(sess.id, epoch, seq, x, y, 0)
            }
            Msg::Yield { epoch } => self.hub.to_input(ToInput::Yield(epoch)),
            Msg::Active { epoch } => self.hub.to_input(ToInput::PeerActive(epoch)),
            Msg::KbdEnter { epoch, prev } => self.injector.kbd_enter_from(sess.id, epoch, prev),
            Msg::KbdLeave { epoch } => self.injector.kbd_leave(sess.id, epoch),
            Msg::ClipForget => self.hub.to_desktop(ToDesktop::PeerForget),
            Msg::ClipNote { kind, detail } => {
                // 对方复制的内容没同步：本机也弹“未同步”（本机关了同步或暂停时不弹，和不接收对方内容一致）
                let st = self.hub.status();
                if self.hub.cfg().clipboard.enabled && !st.paused {
                    use crate::island::{DETAIL_MAX, Kind, Note, clean};
                    let n = Note { who: st.peer_name, kind: Kind::from_code(kind), detail: clean(&detail, DETAIL_MAX), ok: false };
                    self.hub.to_desktop(ToDesktop::Note(n));
                }
            }
            Msg::ClipText { text } => {
                let c = self.hub.cfg().clipboard;
                if c.enabled && text.len() <= c.max_text_kb as usize * 1024 {
                    self.hub.to_desktop(ToDesktop::ClipText(text));
                } else {
                    self.clip_lost();
                }
            }
            Msg::ClipStart { id, format, raw_len } => match clip_limit(&self.hub.cfg().clipboard, format) {
                Some(max) => {
                    if let Err(e) = st.clip.start(id, format, raw_len, max) {
                        crate::log!("忽略对方的剪贴板数据（格式 {format}，{raw_len} 字节，{e}）");
                        self.clip_lost();
                    }
                }
                None => {
                    st.clip.reset();
                    self.clip_lost();
                }
            },
            Msg::Ping { t } => self.enqueue(Out::Msg(Msg::Pong { t })),
            Msg::Rename { name } => self.hub.to_desktop(ToDesktop::Rename(name)),
            Msg::KbdTip => self.hub.to_desktop(ToDesktop::KbdTipGone),
            Msg::Audio { mode, rev } => {
                if let Some(a) = &self.opts.audio {
                    a.peer_mode(sess.id, sess.peer_id, mode, rev);
                }
            }
            Msg::AudioAck { playing } => {
                if let Some(a) = &self.opts.audio {
                    a.peer_ack(playing);
                }
            }
            Msg::Pong { .. } => {}
        }
    }

    /// 对方的排布（对方视角）：rev 更新则镜像后采用；只改排布字段。写盘由 flush 限频。
    fn apply_layout(&self, sess: &Session, theirs: Layout, rev: u64, st: &mut RxState) {
        if !layout_sane(&theirs, rev, config::now_ms()) {
            crate::log!("忽略对方不合理的排布（rev {rev}，错开 {}，缩放 {}）", theirs.offset, theirs.scale);
            return;
        }
        let mine = theirs.mirrored();
        let new_cfg = {
            let mut c = self.hub.cfg.write().unwrap_or_else(|e| e.into_inner());
            if !adopt_layout(c.layout.rev, &c.layout.layout, &mine, rev, self.opts.id, sess.peer_id) {
                return;
            }
            c.layout.layout = mine;
            c.layout.rev = rev;
            c.clone()
        };
        crate::log!("采用对方的排布（rev {rev}）：对方在本机{}，错开 {}", mine.side.label_zh(), mine.offset);
        if self.opts.persist {
            st.layout = Some((mine, rev));
        }
        self.hub.to_input(ToInput::Config(Box::new(new_cfg)));
    }

    /// 把攒着的 peer.json / 排布写盘：每样每秒至多一次（force = 会话结束，写掉剩下的）。内容没变时 config 那边不写。
    fn flush(&self, st: &mut RxState, force: bool) {
        let now = Instant::now();
        let due = |at: Option<Instant>| force || at.is_none_or(|t| now.saturating_duration_since(t) >= PERSIST_EVERY);
        if st.desk.is_some() && due(st.desk_at) {
            if let Some((name, desk, dpis)) = st.desk.take() {
                config::save_peer_desk(&name, &desk, &dpis);
                st.desk_at = Some(now);
            }
        }
        if st.layout.is_some() && due(st.layout_at) {
            if let Some((mine, rev)) = st.layout.take() {
                let r = config::update(&self.hub.cfg(), move |c| {
                    // 磁盘上已有更新的排布（设置窗口刚存的）就不覆盖
                    if c.layout.rev <= rev {
                        c.layout.layout = mine;
                        c.layout.rev = rev;
                    }
                });
                if let Err(e) = r {
                    crate::log!("保存配置失败：{e}");
                }
                st.layout_at = Some(now);
            }
        }
    }

    // ------------------------------------------------------------ 线程

    fn run_listener(self: Arc<Self>) {
        let port = self.hub.cfg().port;
        let listener = loop {
            match TcpListener::bind((self.opts.bind_ip, port)) {
                Ok(l) => break l,
                Err(e) => {
                    self.note_err(format!("TCP 端口 {port} 监听失败：{e}"));
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        };
        crate::log!("监听端口 {port}");
        for conn in listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(e) => {
                    crate::log!("accept 失败：{e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let Ok(peer) = stream.peer_addr() else { continue };
            let ip = peer.ip();
            let peer_cfg = self.hub.cfg.read().unwrap_or_else(|e| e.into_inner()).peer.trim().to_string();
            let reserved = self.known_ip(ip, &peer_cfg);
            if !lock(&self.admit).admit(ip, reserved) {
                continue; // 直接丢弃（关闭）
            }
            let me = self.clone();
            let ok = spawn_named("net-conn", move || {
                let est = me.establish(stream, None);
                lock(&me.admit).release(ip);
                if let Some((sess, s, opener, first)) = est {
                    me.run_reader(sess, s, opener, first);
                }
            });
            if !ok {
                lock(&self.admit).release(ip);
            }
        }
    }

    fn run_dialer(self: Arc<Self>) {
        loop {
            let jitter = Duration::from_millis(crypto::random32()[0] as u64 * 500 / 256);
            std::thread::sleep(self.opts.dial_every + jitter);
            if self.current().is_some() {
                continue;
            }
            let cfg = self.hub.cfg();
            let peer = cfg.peer.trim().to_string();
            if peer.is_empty() || self_blocked(&mut lock(&self.self_peer), &peer, Instant::now()) {
                continue;
            }
            if self.psk(&cfg).is_none() {
                continue;
            }
            let addrs = match resolve_peer(&peer, cfg.port) {
                Ok(a) => a,
                Err(e) => {
                    self.note_err(format!("对方地址 {peer} 解析失败：{e}"));
                    continue;
                }
            };
            // 本机只监听 IPv4（对方也是），IPv6 地址连不上
            let v4: Vec<SocketAddr> = addrs.iter().copied().filter(|a| a.is_ipv4()).collect();
            *lock(&self.peer_ips) = v4.iter().map(|a| a.ip()).collect();
            if v4.is_empty() {
                self.note_err(format!("对方地址 {peer} 只有 IPv6，kmlink 只支持 IPv4，请填对方的 IPv4 地址"));
                continue;
            }
            for addr in v4 {
                match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
                    Ok(s) => {
                        if let Some((sess, s, opener, first)) = self.establish(s, Some(&peer)) {
                            let me = self.clone();
                            let sess2 = sess.clone();
                            if !spawn_named("net-conn", move || me.run_reader(sess, s, opener, first)) {
                                self.end_session(&sess2, "创建线程失败");
                            }
                        }
                        break;
                    }
                    Err(e) => self.note_err(format!("连接 {addr} 失败：{e}")),
                }
            }
        }
    }

    fn run_udp(self: Arc<Self>, port: u16) {
        // 收鼠标位置并当场注入：一来包就要马上处理
        crate::sys::boost_thread(true);
        let sock = loop {
            if let Some(s) = self.udp.get() {
                break s;
            }
            std::thread::sleep(UDP_RETRY);
            match bind_udp(self.opts.bind_ip, port) {
                Ok(s) => {
                    crate::log!("UDP 端口 {port} 绑定成功");
                    let _ = self.udp.set(s);
                }
                Err(_) => continue,
            }
        };
        let mut buf = [0u8; UDP_BUF];
        let mut fails = 0u32;
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(x) => {
                    fails = 0;
                    x
                }
                Err(e) if per_datagram_err(&e) => continue,
                Err(e) => {
                    // 持续出错才退避
                    fails = fails.saturating_add(1);
                    if fails >= 16 {
                        if fails == 16 {
                            crate::log!("UDP 接收持续出错：{e}");
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    continue;
                }
            };
            // 先过滤再解密：长度不对的、不是会话对端 IP 发来的（端口可以变）直接丢
            // 计数器最高位为 1 的是声音包（另一套密钥），解密后只交给声音模块的无锁环，不在这里多做事。
            let audio = crypto::is_audio(&buf[..n]);
            let ok_len = if audio {
                (AUDIO_MIN_PKT..=AUDIO_MAX_PKT).contains(&n)
            } else {
                (UDP_MIN_PKT..=UDP_MAX_PKT).contains(&n)
            };
            if !ok_len {
                continue;
            }
            let Some(sess) = self.current() else { continue };
            if src.ip() != sess.peer.ip() {
                continue;
            }
            if audio {
                let Some(a) = &self.opts.audio else { continue };
                let arrive = crate::smooth::now_us() as u32;
                let Some(plain) = lock(&sess.audio_rx).open(&buf[..n]) else { continue };
                lock(&sess.udp_peer).heard(src, Instant::now());
                a.packet(sess.id, &plain, arrive);
                continue;
            }
            let Some(plain) = lock(&sess.udp_rx).open(&buf[..n]) else { continue };
            lock(&sess.udp_peer).heard(src, Instant::now());
            if let Some(UdpMsg::Move { epoch, seq, x, y, t }) = UdpMsg::decode(&plain) {
                self.mark_mouse();
                #[cfg(test)]
                self.stats.udp_moves.fetch_add(1, Ordering::Relaxed);
                self.injector.moved(sess.id, epoch, seq, x, y, t);
            }
        }
    }

    /// audio-cap 线程：加密发一包声音。会话已换 / UDP 不通（声音不走 TCP）→ false。
    fn send_audio(&self, sid: u64, plain: &[u8]) -> bool {
        let Some(sess) = self.current() else { return false };
        if sess.id != sid || !sess.udp_live() {
            return false;
        }
        let Some(sock) = self.udp.get() else { return false };
        let Some(addr) = lock(&sess.udp_peer).addr else { return false };
        let Some(pkt) = lock(&sess.audio_tx).seal(plain) else { return false };
        sock.send_to(&pkt, addr).is_ok()
    }

    fn send_udp(&self, s: &Session, m: &UdpMsg) {
        if let Some(sock) = self.udp.get() {
            s.send_udp(sock, m);
        }
    }

    /// 对方的剪贴板内容没能落到本机（拒收、超限、出错、超时）：两边都不再一致，各自清掉“已同步”记录。
    fn clip_lost(&self) {
        self.hub.to_desktop(ToDesktop::ClipForget);
        self.enqueue(Out::Msg(Msg::ClipForget));
    }

    fn mark_mouse(&self) {
        self.mouse_at.store(mono_ms(), Ordering::Relaxed);
    }

    fn mouse_active(&self) -> bool {
        let t = self.mouse_at.load(Ordering::Relaxed);
        t != 0 && mono_ms().saturating_sub(t) < MOUSE_ACTIVE_MS
    }

    /// 放进可靠队列，叫醒 net-tcp。
    fn enqueue(&self, o: Out) {
        lock(&self.tq).push(o);
        self.tq_cv.notify_one();
    }

    /// 发鼠标位置：UDP；UDP 不可用或不通时也走 TCP（进可靠队列，接收方按 seq 去重）。
    fn send_move(&self, s: &Session, m: &UdpMsg) {
        let UdpMsg::Move { epoch, seq, x, y, .. } = *m else { return };
        let (udp, tcp) = move_route(self.udp.get().is_some(), s.udp_live());
        if udp {
            self.send_udp(s, m);
        }
        if tcp {
            self.enqueue(Out::Move { epoch, seq, x, y });
        }
    }

    /// UDP 不通时在状态里说明；通了就清掉。
    fn check_udp(&self, s: &Session) {
        let bound = self.udp.get().is_some();
        let live = s.udp_live();
        let shown = self.hub.status().last_error == ERR_UDP_DOWN;
        if !bound || (!live && s.started.elapsed() >= UDP_GRACE) {
            if !s.udp_warned.swap(true, Ordering::Relaxed) {
                crate::log!("与 {} 的 UDP 不通（本机{}绑定），鼠标移动改走 TCP", s.peer, if bound { "已" } else { "未" });
            }
            if !shown {
                self.hub.set_status(|st| st.last_error = ERR_UDP_DOWN.into());
            }
        } else if live && shown {
            self.hub.set_status(|st| st.last_error.clear());
        }
    }

    /// 对方 1.5 秒没有任何帧：抬起本机（被控端）按着的键和按钮，会话不断。有新帧来才重新计时。
    /// 在 last_rx 锁内调用，和 reader 更新时间戳互斥：不会抬起刚收到的按键。
    fn check_stale(&self, s: &Session, now: Instant) {
        let mut c = lock(&s.last_rx);
        if !c.staled && now.saturating_duration_since(c.at) >= STALE_AFTER {
            c.staled = true;
            #[cfg(test)]
            self.stats.stales.fetch_add(1, Ordering::Relaxed);
            self.injector.stale(s.id);
            // 只转发键盘时字会打进黑洞（看不出来）：本机这边也立刻收回键盘
            self.hub.to_input(ToInput::PeerStale);
        }
        if !c.homed && now.saturating_duration_since(c.at) >= GOHOME_AFTER {
            c.homed = true;
            self.hub.to_input(ToInput::GoHome); // 不在控制对方时 input 什么都不做
        }
    }

    /// 每秒一次：设置变了就断开；否则 Ping、UDP Hi、检查 UDP。
    fn on_tick(&self, s: &Session) {
        if CfgSig::of(&self.hub.cfg()) != s.sig {
            self.end_session(s, "密钥/对方地址/端口已修改，用新设置重连");
            return;
        }
        self.enqueue(Out::Msg(Msg::Ping { t: config::now_ms() }));
        self.send_udp(s, &UdpMsg::Hi);
        self.check_udp(s);
    }

    /// 分拣一个要发的东西。
    fn route(self: &Arc<Self>, o: Out, mover: &mut Mover) {
        match o {
            Out::Move { epoch, seq, x, y } => {
                self.mark_mouse();
                mover.push(UdpMsg::Move { epoch, seq, x, y, t: crate::smooth::stamp() })
            }
            Out::CancelClip => {
                self.clip_gen.fetch_add(1, Ordering::Relaxed);
                self.enqueue(Out::CancelClip);
            }
            Out::CancelClipSend => self.enqueue(Out::CancelClip),
            Out::ClipStream { format, raw_len, data } => {
                // 压缩线程交来新分片 / 结束时叫醒 net-tcp。先拿一下队列锁再通知：net-tcp 查过“没活”到开始等待
                // 之间一直持有这把锁，所以通知不会落在两者之间丢掉
                let w = Arc::downgrade(self);
                data.set_waker(Arc::new(move || {
                    if let Some(n) = w.upgrade() {
                        drop(lock(&n.tq));
                        n.tq_cv.notify_one();
                    }
                }));
                self.enqueue(Out::ClipStream { format, raw_len, data });
            }
            o => self.enqueue(o),
        }
    }

    /// 常驻 net-out：独占 hub 的 net 通道，整个进程生命周期。不写 TCP（不会被慢网络卡住）。
    fn run_out(self: Arc<Self>, rx: Receiver<Out>) {
        // 发鼠标位置：钩子交来就要马上发出去
        crate::sys::boost_thread(true);
        let mut next_tick = Instant::now() + PING_EVERY;
        let mut mover = Mover::default();
        loop {
            let mut wake = next_tick;
            if let Some(t) = mover.wake() {
                wake = wake.min(t);
            }
            if let Some(s) = self.current() {
                let c = lock(&s.last_rx);
                if !c.staled {
                    wake = wake.min(c.at + STALE_AFTER);
                } else if !c.homed {
                    wake = wake.min(c.at + GOHOME_AFTER);
                }
            }
            match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(o) => {
                    self.route(o, &mut mover);
                    while let Ok(o) = rx.try_recv() {
                        self.route(o, &mut mover);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    lock(&self.tq).quit = true;
                    self.tq_cv.notify_all();
                    return;
                }
            }

            let now = Instant::now();
            let tick = now >= next_tick;
            let sess = self.current();
            match &sess {
                None => mover = Mover::default(),
                Some(s) => {
                    if let Some(m) = mover.poll(now, tick, s.id) {
                        self.send_move(s, &m);
                    }
                    self.check_stale(s, now);
                }
            }
            if tick {
                next_tick = now + PING_EVERY;
                if let Some(s) = &sess {
                    self.on_tick(s);
                }
            }
        }
    }

    /// 常驻 net-tcp：从可靠队列取，阻塞写 TCP。每轮先发全部消息，再发至多一个图片分片（压缩流暂时没有分片就等）。
    fn run_tcp(self: Arc<Self>) {
        let mut img_id = u32::from_le_bytes([crypto::random32()[0], 0, 0, 0]);
        // 鼠标正在两台之间移动时，图片分片限速发（PACE_BPS）：分片发得太快会把 WiFi 占满，
        // UDP 移动包在网卡/路由器队列里排在图片后面（实测传 14MB 图期间跨屏停顿从 6ms 涨到 90ms）。消息不受限。
        let mut next_chunk = Instant::now();
        loop {
            let allow_chunk;
            {
                let mut q = lock(&self.tq);
                loop {
                    if q.quit {
                        return;
                    }
                    let now = Instant::now();
                    let paced = now < next_chunk && self.mouse_active();
                    if q.has_msgs() {
                        allow_chunk = !paced;
                        break;
                    }
                    if q.has_work() {
                        if paced {
                            q = self.tq_cv.wait_timeout(q, next_chunk - now).unwrap_or_else(|e| e.into_inner()).0;
                            continue;
                        }
                        allow_chunk = true;
                        break;
                    }
                    q = self.tq_cv.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            }
            let Some(s) = self.current() else {
                lock(&self.tq).clear();
                continue;
            };
            let (frames, chunk) = lock(&self.tq).take_batch_paced(s.id, &mut img_id, allow_chunk);
            if chunk > 0 && self.mouse_active() {
                next_chunk = Instant::now() + Duration::from_secs_f64(chunk as f64 / PACE_BPS);
            }
            for f in frames {
                if let Err(e) = s.send_plain(&f) {
                    if e.kind() == io::ErrorKind::InvalidInput {
                        crate::log!("丢弃过大的消息（{} 字节）", f.len());
                        continue;
                    }
                    crate::log!("发送失败：{e}");
                    lock(&self.tq).clear();
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 发送队列（纯逻辑）

struct ImgTx {
    /// 第一次发送时绑定的会话；会话变了就放弃。
    sess: Option<u64>,
    id: u32,
    format: u32,
    raw_len: u32,
    /// 压缩流：丢弃 ImgTx 即告诉压缩线程不要了。
    data: StreamRx,
    header_sent: bool,
    /// 已发出的压缩字节数。
    off: usize,
}

#[derive(Default)]
struct OutQueue {
    msgs: VecDeque<Msg>,
    img: Option<ImgTx>,
    /// 已发出开始帧、没发完就被丢弃的流：(会话, id)，下一轮在同一会话里发 FRAME_ABORT。
    abort: Option<(u64, u32)>,
    /// hub 通道已关闭（进程退出）：net-tcp 结束。
    quit: bool,
}

impl OutQueue {
    fn push(&mut self, o: Out) {
        match o {
            Out::Msg(m) => {
                // 新复制的文字顶掉还没发完的图片/大段文字，免得对方最后收到旧内容
                if matches!(m, Msg::ClipText { .. }) {
                    self.drop_img();
                }
                self.msgs.push_back(m)
            }
            // 走 TCP 的鼠标位置（UDP 不可用/不通时）：只留最新的一个；排在已排队的消息之后（Enter 先到），图片分片之前
            Out::Move { epoch, seq, x, y } => {
                self.msgs.retain(|m| !matches!(m, Msg::Move { .. }));
                self.msgs.push_back(Msg::Move { epoch, seq, x, y });
            }
            Out::ClipStream { format, raw_len, data } => {
                // 新图片顶掉还没发完的旧图片（接收方见到新 id 也会丢弃旧的）
                self.drop_img();
                if raw_len == 0 {
                    return; // data 随之丢弃，压缩线程停下
                }
                self.img = Some(ImgTx { sess: None, id: 0, format, raw_len, data, header_sent: false, off: 0 });
            }
            Out::CancelClip | Out::CancelClipSend => self.drop_img(),
        }
    }

    /// 丢掉正在发的流（压缩线程随之停下）；对方已开始接收的，记下要补发作废。
    fn drop_img(&mut self) {
        if let Some(i) = self.img.take()
            && i.header_sent
            && let Some(s) = i.sess
        {
            self.abort = Some((s, i.id));
        }
    }

    /// 有消息要发（不受图片限速影响）。
    fn has_msgs(&self) -> bool {
        !self.msgs.is_empty() || self.abort.is_some()
    }

    fn has_work(&self) -> bool {
        !self.msgs.is_empty() || self.abort.is_some() || self.img.as_ref().is_some_and(|i| !i.header_sent || i.data.ready())
    }

    fn clear(&mut self) {
        self.msgs.clear();
        self.img = None;
        self.abort = None;
        if self.msgs.capacity() > 256 {
            self.msgs.shrink_to_fit();
        }
    }

    /// 取出这一轮要发的 TCP 明文帧：全部消息、待补发的作废，然后至多一个图片帧（开始、一个分片、结束或作废）。
    #[cfg(test)]
    fn take_batch(&mut self, sess_id: u64, img_id: &mut u32) -> Vec<Vec<u8>> {
        self.take_batch_paced(sess_id, img_id, true).0
    }

    /// 取一批要发的帧：全部消息 + （allow_chunk 时）至多一个图片分片。返回 (帧, 本批图片分片的数据字节数)。
    fn take_batch_paced(&mut self, sess_id: u64, img_id: &mut u32, allow_chunk: bool) -> (Vec<Vec<u8>>, usize) {
        let frames = self.take_inner(sess_id, img_id, allow_chunk);
        let chunk = frames.iter().filter(|f| f.first() == Some(&proto::FRAME_CHUNK)).map(|f| f.len()).sum();
        (frames, chunk)
    }

    fn take_inner(&mut self, sess_id: u64, img_id: &mut u32, allow_chunk: bool) -> Vec<Vec<u8>> {
        let mut frames: Vec<Vec<u8>> = self.msgs.drain(..).map(|m| proto::encode_msg(&m)).collect();
        if self.msgs.capacity() > 256 {
            self.msgs.shrink_to_fit();
        }
        // 旧会话的作废不用发（对方那边的会话已经没了）
        if let Some((s, id)) = self.abort.take()
            && s == sess_id
        {
            frames.push(proto::encode_abort(id));
        }
        if self.img.as_ref().is_some_and(|i| i.sess.is_some_and(|s| s != sess_id)) {
            self.img = None;
        }
        let Some(img) = self.img.as_mut() else { return frames };
        img.sess = Some(sess_id);
        if !img.header_sent {
            *img_id = img_id.wrapping_add(1);
            img.id = *img_id;
            img.header_sent = true;
            frames.push(proto::encode_msg(&Msg::ClipStart { id: img.id, format: img.format, raw_len: img.raw_len }));
            return frames;
        }
        if !allow_chunk {
            return frames;
        }
        match img.data.take() {
            Piece::Data(v) => match u32::try_from(img.off) {
                Ok(off) if u32::try_from(img.off + v.len()).is_ok() => {
                    frames.push(proto::encode_chunk(img.id, off, &v));
                    img.off += v.len();
                }
                _ => {
                    frames.push(proto::encode_abort(img.id));
                    self.img = None;
                }
            },
            Piece::End => {
                frames.push(proto::encode_end(img.id, img.off as u32));
                self.img = None;
            }
            Piece::Gone => {
                frames.push(proto::encode_abort(img.id));
                self.img = None;
            }
            Piece::Wait => {}
        }
        frames
    }
}

// ---------------------------------------------------------------- 剪贴板压缩流的接收（纯逻辑）

/// 正在接收的一条流：最终的内存一次分配好，每片直接解压进去，边解边算摘要。
struct Incoming {
    id: u32,
    format: u32,
    raw_len: usize,
    /// raw_len 字节（文字另加 2 字节结尾 0），收齐后整块交给剪贴板。
    mem: crate::clip::Global,
    z: proto::Inflater,
    digest: crate::clip::Digest,
    /// 已收到的压缩字节数。
    off: usize,
    /// 最近一次有进展的时间（30 秒不动就释放）。
    at: Instant,
}

impl Incoming {
    /// 压缩数据至多这么长（level 1 对不可压缩数据只多出很少）：防止对方用不出数据的空块拖住。
    fn max_compressed(&self) -> usize {
        self.raw_len + self.raw_len / 16 + 4096
    }

    fn feed(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        if offset as usize != self.off {
            return Err("分片错位");
        }
        self.off += data.len();
        if self.off > self.max_compressed() {
            return Err("压缩数据过长");
        }
        let Incoming { mem, z, digest, raw_len, .. } = self;
        mem.with(|b| {
            let out = &mut b[..*raw_len];
            z.feed(data, out)?;
            digest.update(&out[..z.written()])
        })
        .ok_or("内存锁定失败")??;
        self.at = Instant::now();
        Ok(())
    }

    /// 结束帧：核对总长与解压长度，做完整的 DIB 校验，算完摘要。
    fn finish(self, total: u32) -> Result<crate::clip::Blob, &'static str> {
        if total as usize != self.off {
            return Err("压缩数据总长不符");
        }
        if !self.z.done() || self.z.written() != self.raw_len {
            return Err("解压后的长度不符");
        }
        let Incoming { format, raw_len, mut mem, digest, .. } = self;
        let (hash, text_len) = mem
            .with(|b| {
                let data = &b[..raw_len];
                if format != crate::clip::CF_UNICODETEXT {
                    crate::clip::dib(data)?;
                }
                digest.finish(data)
            })
            .ok_or("内存锁定失败")??;
        Ok(crate::clip::Blob { format, mem, hash, text_len })
    }
}

#[derive(Default)]
struct ClipRx {
    cur: Option<Incoming>,
    /// 对方的内容因出错/超时被丢弃（reader 看到后让两边都清掉“已同步”记录）。
    lost: bool,
}

impl ClipRx {
    /// 丢掉正在接收的（之后这个 id 的分片都不认），内存立刻释放。
    fn reset(&mut self) {
        self.cur = None;
    }

    /// 新传输开始（先丢弃未完成的旧传输）：按 raw_len 一次分配好最终内存。
    /// 只收图片（CF_DIB / CF_DIBV5）和大段文字（CF_UNICODETEXT，长度须为偶数）；超过上限、分配失败：Err。
    fn start(&mut self, id: u32, format: u32, raw_len: u32, max: usize) -> Result<(), &'static str> {
        use crate::clip::{CF_DIB, CF_DIBV5, CF_UNICODETEXT, Digest, Global};
        self.cur = None;
        let raw_len = raw_len as usize;
        let text = format == CF_UNICODETEXT;
        if !text && format != CF_DIB && format != CF_DIBV5 {
            return Err("格式不支持");
        }
        if raw_len == 0 || raw_len > max {
            return Err("超过上限");
        }
        if text && raw_len % 2 != 0 {
            return Err("文字长度不对");
        }
        // 文字另留 2 字节结尾 0（CF_UNICODETEXT 要求）
        let size = if text { raw_len + 2 } else { raw_len };
        let mut mem = Global::alloc(size).ok_or("内存分配失败")?;
        if text {
            mem.with(|b| b[raw_len..].fill(0)).ok_or("内存锁定失败")?;
        }
        let digest = if text { Digest::text() } else { Digest::image(raw_len) };
        self.cur = Some(Incoming { id, format, raw_len, mem, z: proto::Inflater::new(), digest, off: 0, at: Instant::now() });
        Ok(())
    }

    /// 半截的传输 30 秒没进展：释放。
    fn expire(&mut self, now: Instant) {
        if self.cur.as_ref().is_some_and(|c| now.saturating_duration_since(c.at) >= CLIP_STALL) {
            crate::log!("对方的剪贴板数据 30 秒没有进展，丢弃");
            self.cur = None;
            self.lost = true;
        }
    }

    /// 收到一片：直接解压进最终内存。出错（错位、损坏、超长、图片头不对）就整条丢弃。
    fn chunk(&mut self, id: u32, offset: u32, data: &[u8]) {
        let Some(inc) = self.cur.as_mut() else { return };
        if inc.id != id {
            return; // 旧传输的残片
        }
        if let Err(e) = inc.feed(offset, data) {
            crate::log!("对方的剪贴板数据有误（{e}），丢弃");
            self.cur = None;
            self.lost = true;
        }
    }

    /// 结束帧：一切核对无误时交出整块内容。
    fn end(&mut self, id: u32, total: u32) -> Option<crate::clip::Blob> {
        if self.cur.as_ref()?.id != id {
            return None;
        }
        match self.cur.take()?.finish(total) {
            Ok(b) => Some(b),
            Err(e) => {
                crate::log!("对方的剪贴板数据有误（{e}），丢弃");
                self.lost = true;
                None
            }
        }
    }

    /// 对方作废了这条流。
    fn abort(&mut self, id: u32) {
        if self.cur.as_ref().is_some_and(|c| c.id == id) {
            self.cur = None;
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Side;
    use crate::hub::{HubRx, ToDesktop, ToInput};
    use crate::proto::CHUNK;

    fn psk() -> [u8; 32] {
        [0x42; 32]
    }

    fn no_reflect(_: &[u8; 32]) -> bool {
        false
    }

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (a, b)
    }

    fn hs_pair(kc: [u8; 32], ks: [u8; 32]) -> (TcpStream, Chans, TcpStream, Chans) {
        let (mut c, mut s) = tcp_pair();
        let t = std::thread::spawn(move || {
            let ch = handshake(&mut s, &ks, Role::Server(&no_reflect)).unwrap();
            (s, ch)
        });
        let cc = handshake(&mut c, &kc, Role::Client(crypto::random32())).unwrap();
        let (s, sc) = t.join().unwrap();
        (c, cc, s, sc)
    }

    #[test]
    fn handshake_and_framing_over_tcp() {
        let (mut c, mut cc, mut s, mut sc) = hs_pair(psk(), psk());
        assert_eq!(cc.nonce_c, sc.nonce_c);
        // c → s
        let m = Msg::Key { epoch: 3, scan: 0x1e, ext: false, vk: 0x41, down: true };
        write_frame(&mut c, &mut cc.tx, &proto::encode_msg(&m)).unwrap();
        write_frame(&mut c, &mut cc.tx, &proto::encode_chunk(9, 0, &[7u8; CHUNK])).unwrap();
        match proto::decode_frame(&read_frame(&mut s, &mut sc.rx, MAX_FRAME).unwrap()) {
            Some(Frame::Msg(d)) => assert_eq!(d, m),
            _ => panic!("expected msg"),
        }
        match proto::decode_frame(&read_frame(&mut s, &mut sc.rx, MAX_FRAME).unwrap()) {
            Some(Frame::Chunk { id, offset, data }) => assert_eq!((id, offset, data.len()), (9, 0, CHUNK)),
            _ => panic!("expected chunk"),
        }
        // s → c
        write_frame(&mut s, &mut sc.tx, &proto::encode_msg(&Msg::Yield { epoch: 0 })).unwrap();
        assert!(matches!(proto::decode_frame(&read_frame(&mut c, &mut cc.rx, MAX_FRAME).unwrap()), Some(Frame::Msg(Msg::Yield { .. }))));
        // 篡改：直接写一个用错误密钥/计数器加密的帧
        let mut fresh = Sealer::new(&crypto::derive(&psk(), &[0; 32], &cc.nonce_c, &[0; 32]).c2s);
        write_frame(&mut c, &mut fresh, b"\0{}").unwrap();
        assert_eq!(read_frame(&mut s, &mut sc.rx, MAX_FRAME).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// 记下经过的字节（模拟窃听者录下的握手）。
    struct Tap<S> {
        s: S,
        wrote: Vec<u8>,
        read: Vec<u8>,
    }
    impl<S: Read> Read for Tap<S> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let n = self.s.read(b)?;
            self.read.extend_from_slice(&b[..n]);
            Ok(n)
        }
    }
    impl<S: Write> Write for Tap<S> {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let n = self.s.write(b)?;
            self.wrote.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.s.flush()
        }
    }

    #[test]
    fn kml2_handshake_forward_secrecy() {
        let (c, mut s) = tcp_pair();
        let t = std::thread::spawn(move || handshake(&mut s, &psk(), Role::Server(&no_reflect)).unwrap());
        let mut tap = Tap { s: c, wrote: Vec::new(), read: Vec::new() };
        let mut cc = handshake(&mut tap, &psk(), Role::Client(crypto::random32())).unwrap();
        let mut sc = t.join().unwrap();
        // 线上：双方各 68 字节 = KML2 + nonce + X25519 临时公钥
        assert_eq!((tap.wrote.len(), tap.read.len()), (68, 68));
        assert_eq!(&tap.wrote[..4], b"KML2");
        assert_eq!(&tap.read[..4], b"KML2");
        let nc: [u8; 32] = tap.wrote[4..36].try_into().unwrap();
        let ns: [u8; 32] = tap.read[4..36].try_into().unwrap();
        let (pkc, pks): ([u8; 32], [u8; 32]) = (tap.wrote[36..].try_into().unwrap(), tap.read[36..].try_into().unwrap());
        assert_eq!(nc, cc.nonce_c);
        assert_ne!(pkc, pks);
        let msg = proto::encode_msg(&Msg::Yield { epoch: 7 });
        let mut ct = Vec::new();
        write_frame(&mut ct, &mut cc.tx, &msg).unwrap();
        // 日后拿到 psk + 录下的握手（没有临时私钥）：不带 DH（旧算法）或拿公钥当 DH 都解不开
        for guess in [[0u8; 32], pkc, pks] {
            let k = crypto::derive(&psk(), &guess, &nc, &ns);
            assert!(read_frame(&mut &ct[..], &mut Opener::new(&k.c2s), MAX_FRAME).is_err());
        }
        // 真正的对端能解开
        assert_eq!(read_frame(&mut &ct[..], &mut sc.rx, MAX_FRAME).unwrap(), msg);
        // 同一个 psk 再握手一次：会话密钥不同
        let (_, mut cc2, _, _) = hs_pair(psk(), psk());
        let mut ct2 = Vec::new();
        write_frame(&mut ct2, &mut cc2.tx, &msg).unwrap();
        assert_ne!(ct, ct2);
    }

    #[test]
    fn old_version_magic_reported() {
        let (mut c, mut s) = tcp_pair();
        c.write_all(b"KML1").unwrap();
        c.write_all(&[0u8; 32]).unwrap();
        let e = handshake(&mut s, &psk(), Role::Server(&no_reflect)).err().unwrap();
        assert!(is_err(&e, ERR_OLD), "{e:?}");
    }

    #[test]
    fn reflected_nonce_rejected_before_auth() {
        let t0 = Instant::now();
        let mut sent = SentNonces::default();
        let nc = crypto::random32();
        sent.remember(nc, t0);
        assert!(!sent.hit(&nc));
        assert!(!sent.reflected(&[1u8; 32], t0));
        assert!(sent.reflected(&nc, t0 + Duration::from_secs(5)));
        assert!(sent.hit(&nc));
        // 10 秒后忘掉
        assert!(!sent.reflected(&nc, t0 + SENT_NC_KEEP));
        // 至多 16 个
        for i in 0..40u8 {
            sent.remember([i; 32], t0);
        }
        assert_eq!(sent.0.len(), SENT_NC_MAX);

        // 握手：服务端收到本机刚发出的 nonce_c → 不回握手，直接拒绝；客户端看到连接断开，并能查到“被本机收到过”
        let shared = Arc::new(Mutex::new(SentNonces::default()));
        let nc = crypto::random32();
        lock(&shared).remember(nc, Instant::now());
        let (mut c, mut s) = tcp_pair();
        let sh = shared.clone();
        let t = std::thread::spawn(move || {
            let seen = |n: &[u8; 32]| lock(&sh).reflected(n, Instant::now());
            handshake(&mut s, &psk(), Role::Server(&seen)).err().unwrap()
        });
        let ce = handshake(&mut c, &psk(), Role::Client(nc)).err().unwrap();
        let se = t.join().unwrap();
        assert!(is_err(&se, ERR_REFLECT), "{se:?}");
        assert!(is_closed(&ce), "{ce:?}");
        assert!(lock(&shared).hit(&nc));
    }

    #[test]
    fn wrong_key_fails_first_frame() {
        let (mut c, mut cc, mut s, mut sc) = hs_pair(psk(), [0x43; 32]);
        write_frame(&mut c, &mut cc.tx, &proto::encode_msg(&Msg::Yield { epoch: 0 })).unwrap();
        assert_eq!(read_frame(&mut s, &mut sc.rx, MAX_FRAME).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_and_bad_magic_rejected() {
        let (mut c, mut s) = tcp_pair();
        c.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes()).unwrap();
        let mut o = Opener::new(&psk());
        assert_eq!(read_frame(&mut s, &mut o, MAX_FRAME).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut sealer = Sealer::new(&psk());
        assert_eq!(write_frame(&mut c, &mut sealer, &vec![0u8; MAX_PLAIN + 1]).unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let (mut c, mut s) = tcp_pair();
        c.write_all(b"XXXX").unwrap();
        c.write_all(&[0u8; 64]).unwrap();
        assert!(handshake(&mut s, &psk(), Role::Server(&no_reflect)).is_err());
        // 小阶点（全 0 公钥）：拒绝
        let (mut c, mut s) = tcp_pair();
        c.write_all(MAGIC).unwrap();
        c.write_all(&[0u8; 64]).unwrap();
        let e = handshake(&mut s, &psk(), Role::Server(&no_reflect)).err().unwrap();
        assert!(is_err(&e, ERR_PEER_KEY), "{e:?}");
    }

    #[test]
    fn udp_seal_open_over_sockets() {
        let (_, cc, _, sc) = hs_pair(psk(), psk());
        let (mut utx, mut urx) = (cc.utx, sc.urx);
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mv = UdpMsg::Move { epoch: 1, seq: 5, x: 100, y: -20, t: 7 };
        let p1 = utx.seal(&mv.encode()).unwrap();
        let p2 = utx.seal(&UdpMsg::Hi.encode()).unwrap();
        // 合法包的长度都在过滤范围内
        assert!((UDP_MIN_PKT..=UDP_MAX_PKT).contains(&p1.len()));
        assert!((UDP_MIN_PKT..=UDP_MAX_PKT).contains(&p2.len()));
        a.send_to(&p2, b.local_addr().unwrap()).unwrap();
        a.send_to(&p1, b.local_addr().unwrap()).unwrap(); // 迟到的旧包
        let mut buf = [0u8; 2048];
        let (n, src) = b.recv_from(&mut buf).unwrap();
        assert_eq!(src, a.local_addr().unwrap());
        assert_eq!(UdpMsg::decode(&urx.open(&buf[..n]).unwrap()), Some(UdpMsg::Hi));
        let (n, _) = b.recv_from(&mut buf).unwrap();
        assert!(urx.open(&buf[..n]).is_none());
        // 反方向密钥不同
        let mut wrong = UdpOpener::new(&[0u8; 32]);
        assert!(wrong.open(&p1).is_none());
    }

    #[test]
    fn tie_break() {
        let big = [9u8; 32];
        let small = [1u8; 32];
        assert!(keep_old(Duration::from_millis(500), &big, &small));
        assert!(!keep_old(Duration::from_millis(500), &small, &big));
        assert!(!keep_old(Duration::from_secs(3), &big, &small));
    }

    fn mv(seq: u32) -> Out {
        Out::Move { epoch: 1, seq, x: seq as i32, y: 0 }
    }

    fn kinds(frames: &[Vec<u8>]) -> Vec<String> {
        frames
            .iter()
            .map(|f| match proto::decode_frame(f) {
                Some(Frame::Msg(Msg::ClipStart { .. })) => "hdr".into(),
                Some(Frame::Msg(Msg::Ping { t })) => format!("p{t}"),
                Some(Frame::Msg(Msg::Move { seq, .. })) => format!("m{seq}"),
                Some(Frame::Msg(_)) => "msg".into(),
                Some(Frame::Chunk { offset, .. }) => format!("c{offset}"),
                Some(Frame::End { total, .. }) => format!("end{total}"),
                Some(Frame::Abort { id }) => format!("abort{id}"),
                None => "?".into(),
            })
            .collect()
    }

    /// 测试用的流：pieces 先排好（不限量）；finish = 已送完。返回还没送完时的发送端。
    fn queued_stream(pieces: &[Vec<u8>], finish: bool) -> (Option<crate::hub::StreamTx>, StreamRx) {
        let (tx, rx) = crate::hub::clip_stream(usize::MAX);
        for p in pieces {
            assert!(tx.send(p.clone(), &|| true));
        }
        if finish {
            tx.finish();
            (None, rx)
        } else {
            (Some(tx), rx)
        }
    }

    fn stream_out(format: u32, pieces: &[Vec<u8>]) -> Out {
        Out::ClipStream { format, raw_len: 999, data: queued_stream(pieces, true).1 }
    }

    #[test]
    fn queue_priority_and_coalescing() {
        let mut q = OutQueue::default();
        let mut id = 0u32;
        let pieces = vec![vec![1u8; CHUNK], vec![2u8; CHUNK], vec![3u8; 10]];
        q.push(stream_out(8, &pieces));
        q.push(mv(1));
        q.push(Out::Msg(Msg::Ping { t: 1 }));
        q.push(mv(2));
        q.push(Out::Msg(Msg::Ping { t: 2 }));
        // 走 TCP 的 Move 只留最新的，排在已排队消息之后、图片之前
        assert_eq!(kinds(&q.take_batch(7, &mut id)), ["p1", "m2", "p2", "hdr"]);
        assert_eq!(kinds(&q.take_batch(7, &mut id)), ["c0"]);
        // 分片之间插入的消息先发
        q.push(Out::Msg(Msg::Ping { t: 3 }));
        q.push(mv(3));
        assert_eq!(kinds(&q.take_batch(7, &mut id)), ["p3".to_string(), "m3".into(), format!("c{CHUNK}")]);
        assert_eq!(kinds(&q.take_batch(7, &mut id)), [format!("c{}", CHUNK * 2)]);
        assert_eq!(kinds(&q.take_batch(7, &mut id)), [format!("end{}", CHUNK * 2 + 10)]);
        assert!(q.img.is_none());
        assert!(!q.has_work());
        assert!(q.take_batch(7, &mut id).is_empty());
        // 取消：丢掉待发的图片（还没发开始帧：不用告诉对方）
        q.push(stream_out(8, &pieces));
        q.push(Out::CancelClip);
        assert!(!q.has_work());
    }

    #[test]
    fn queue_waits_for_stream() {
        let mut q = OutQueue::default();
        let mut id = 0u32;
        let (tx, rx) = queued_stream(&[], false);
        let tx = tx.unwrap();
        q.push(Out::ClipStream { format: 17, raw_len: 100, data: rx });
        assert!(q.has_work());
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["hdr"]);
        // 压缩线程还没交分片：没活（net-tcp 等着，不空转）
        assert!(!q.has_work());
        assert!(q.take_batch(1, &mut id).is_empty());
        assert!(tx.send(vec![5; 100], &|| true));
        assert!(q.has_work());
        // 消息照样插在分片前面
        q.push(Out::Msg(Msg::Ping { t: 9 }));
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["p9", "c0"]);
        assert!(!q.has_work());
        tx.finish();
        assert!(q.has_work());
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["end100"]);
        assert!(!q.has_work());

        // 压缩线程中途放弃：告诉对方作废
        let (tx, rx) = queued_stream(&[vec![1; 10]], false);
        q.push(Out::ClipStream { format: 17, raw_len: 100, data: rx });
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["hdr"]);
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["c0"]);
        drop(tx);
        assert!(q.has_work());
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["abort2"]);
        assert!(!q.has_work());

        // 发到一半被取消 / 被新文字顶掉：压缩线程那边立刻知道，下一轮告诉对方作废
        for cancel in [Out::CancelClip, Out::Msg(Msg::ClipText { text: "新".into() })] {
            let (tx, rx) = queued_stream(&[vec![1; 10]], false);
            let tx = tx.unwrap();
            q.push(Out::ClipStream { format: 8, raw_len: 100, data: rx });
            assert_eq!(kinds(&q.take_batch(1, &mut id)), ["hdr"]);
            let n = id;
            q.push(cancel);
            assert!(!tx.send(vec![1], &|| true), "流已被丢弃");
            let k = kinds(&q.take_batch(1, &mut id));
            assert_eq!(k.last().unwrap(), &format!("abort{n}"));
            assert!(!q.has_work());
        }
    }

    #[test]
    fn queue_drops_image_on_session_change() {
        let mut q = OutQueue::default();
        let mut id = 0u32;
        let (tx, rx) = queued_stream(&[vec![1; CHUNK]], false);
        let tx = tx.unwrap();
        q.push(Out::ClipStream { format: 8, raw_len: 1, data: rx });
        assert_eq!(kinds(&q.take_batch(1, &mut id)), ["hdr"]);
        // 会话换了：丢掉（旧会话的作废不用发），压缩线程随之停下
        assert!(q.take_batch(2, &mut id).is_empty());
        assert!(q.img.is_none());
        assert!(!tx.send(vec![1], &|| true));
        // 新图片替换旧图片，id 递增；旧的发过开始帧，先补发作废
        q.push(stream_out(8, &[vec![1; 10]]));
        assert_eq!(kinds(&q.take_batch(2, &mut id)), ["hdr"]);
        q.push(stream_out(17, &[vec![2; 10]]));
        let f = q.take_batch(2, &mut id);
        assert_eq!(kinds(&f), ["abort2", "hdr"]);
        match proto::decode_frame(&f[1]) {
            Some(Frame::Msg(Msg::ClipStart { id, format, raw_len })) => assert_eq!((id, format, raw_len), (3, 17, 999)),
            _ => panic!(),
        }
        // 断开（clear）：什么都不留
        q.clear();
        assert!(!q.has_work());
    }

    #[test]
    fn mover_throttles_but_keeps_latest() {
        let ms = Duration::from_millis;
        let m_ = |s: u32| UdpMsg::Move { epoch: 1, seq: s, x: s as i32, y: 0, t: s };
        let t = Instant::now();
        let mut m = Mover::default();
        assert!(m.wake().is_none());
        m.push(m_(1));
        assert_eq!(m.poll(t, false, 1), Some(m_(1))); // 第一个立刻发
        m.push(m_(2));
        m.push(m_(3));
        assert_eq!(m.poll(t + ms(1), false, 1), None); // 不到 2ms
        assert_eq!(m.wake(), Some(t + MOVE_GAP));
        assert_eq!(m.poll(t + MOVE_GAP, false, 1), Some(m_(3))); // 间隔一到发最新的
        let t2 = t + MOVE_GAP;
        assert_eq!(m.poll(t2 + ms(1), false, 1), None);
        // 100ms 没有新的：补发最后一个，然后每秒的 tick 再补
        assert_eq!(m.wake(), Some(t2 + REPAIR_AFTER));
        assert_eq!(m.poll(t2 + REPAIR_AFTER, false, 1), Some(m_(3)));
        assert_eq!(m.poll(t2 + REPAIR_AFTER + ms(1), false, 1), None);
        assert!(m.wake().is_none());
        assert_eq!(m.poll(t2 + ms(1000), true, 1), Some(m_(3)));
        // 会话变了：不补发旧会话的
        assert_eq!(m.poll(t2 + ms(1500), true, 2), None);
        // 安静够久了：新位置立刻发
        m.push(m_(4));
        assert_eq!(m.poll(t2 + ms(2000), false, 2), Some(m_(4)));
    }

    #[test]
    fn udp_falls_back_to_tcp_and_back() {
        let t0 = Instant::now();
        let src: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let mut u = UdpPeer::default();
        assert!(!u.live(t0));
        u.heard(src, t0);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert!(u.live(at(2000)));
        assert_eq!(move_route(true, u.live(at(2000))), (true, false));
        // 2.5 秒没收到对方的 UDP（对方每秒发 Hi）：鼠标位置回落到 TCP（UDP 照发，探路）
        assert!(!u.live(at(2600)));
        assert!(!u.learned);
        assert_eq!(move_route(true, u.live(at(2600))), (true, true));
        assert_eq!(u.addr, Some(src));
        // 又收到了：切回只走 UDP
        u.heard(src, at(3000));
        assert_eq!(move_route(true, u.live(at(3000))), (true, false));
    }

    #[test]
    fn per_ip_handshake_quota() {
        let ip = |i: u8| IpAddr::V4(Ipv4Addr::new(10, 0, 0, i));
        let mut a = Admission::default();
        assert!(a.admit(ip(1), false));
        assert!(a.admit(ip(1), false));
        assert!(!a.admit(ip(1), false)); // 同一 IP 至多 2 个未认证连接
        assert!(!a.admit(ip(1), true)); // 预留的 IP 也一样
        for i in 2..8 {
            assert!(a.admit(ip(i), false));
        }
        assert_eq!(a.total, MAX_PENDING_HANDSHAKES);
        assert!(!a.admit(ip(20), false)); // 总数满
        assert!(a.admit(ip(30), true)); // cfg.peer / 上次连上的 IP 照样进
        a.release(ip(1));
        assert!(!a.admit(ip(21), false));
        a.release(ip(30));
        assert!(a.admit(ip(21), false));
        a.release(ip(99)); // 没登记过的：无事
        for i in [1, 2, 3, 4, 5, 6, 7, 21] {
            a.release(ip(i));
        }
        assert_eq!(a.total, 0);
        assert!(a.per_ip.is_empty());
    }

    /// 流式压缩成分片（和发送端同一个压缩器）。
    fn deflate(raw: &[u8]) -> Vec<Vec<u8>> {
        crate::proto::tests::deflate_pieces(raw, 0)
    }

    /// 合法的 DIB（CF_DIB 头），像素 n 字节：前 1/3 可压缩，其余随机。
    fn test_dib(n: usize) -> Vec<u8> {
        let w = 100;
        let n = n / (w * 4) * (w * 4);
        let mut d = crate::clip::tests::make_dib(40, w as i32, (n / (w * 4)) as i32, 32, 0, 0, 0, n);
        let k = d.len() / 3;
        getrandom::fill(&mut d[k..]).unwrap();
        d
    }

    /// 把分片按顺序喂给 rx（偏移按压缩数据累计），最后发结束帧。
    fn feed_all(rx: &mut ClipRx, id: u32, pieces: &[Vec<u8>]) -> Option<crate::clip::Blob> {
        let mut off = 0;
        for p in pieces {
            rx.chunk(id, off as u32, p);
            off += p.len();
        }
        rx.end(id, off as u32)
    }

    fn bytes(b: &mut crate::clip::Blob) -> Vec<u8> {
        b.mem.with(|m| m.to_vec()).unwrap()
    }

    #[test]
    fn clip_stream_receive() {
        use crate::clip::{CF_DIB, CF_DIBV5, hash_image};
        let raw = test_dib(300_000);
        let z = deflate(&raw);
        assert!(z.len() > 3);
        let mut rx = ClipRx::default();
        assert!(rx.start(5, CF_DIB, raw.len() as u32, 1 << 20).is_ok());
        assert_eq!(rx.cur.as_ref().unwrap().mem.len(), raw.len()); // 一次按 raw_len 分配最终内存
        let mut b = feed_all(&mut rx, 5, &z).expect("收齐");
        assert!(rx.cur.is_none());
        assert_eq!((b.format, b.text_len), (CF_DIB, 0));
        assert_eq!(b.hash, hash_image(CF_DIB, &raw)); // 边收边算的摘要
        assert!(bytes(&mut b) == raw);
        // 任意切分（含 1 字节一片、恰好跨分片边界）
        let all = z.concat();
        for cut in [1, 333, CHUNK - 1, CHUNK, CHUNK + 1] {
            if all.len() / cut > 50_000 {
                continue;
            }
            let re: Vec<Vec<u8>> = all.chunks(cut).map(|c| c.to_vec()).collect();
            assert!(rx.start(6, CF_DIBV5, raw.len() as u32, 1 << 20).is_ok());
            let mut b = feed_all(&mut rx, 6, &re).expect("收齐");
            assert!(bytes(&mut b) == raw);
        }

        // 超上限、格式不对、长度 0：不分配
        assert!(rx.start(6, CF_DIB, raw.len() as u32, 1000).is_err());
        assert!(rx.cur.is_none());
        assert!(rx.start(6, 2, 100, 1 << 20).is_err());
        assert!(rx.start(6, CF_DIB, 0, 1 << 20).is_err());
        assert!(rx.start(6, 13, 101, 1 << 20).is_err()); // 文字长度须为偶数
        // 新 id 丢弃旧传输；旧 id 的残片、结束都不认
        assert!(rx.start(7, CF_DIB, raw.len() as u32, 1 << 20).is_ok());
        rx.chunk(7, 0, &z[0]);
        assert!(rx.start(8, CF_DIBV5, raw.len() as u32, 1 << 20).is_ok());
        rx.chunk(7, z[0].len() as u32, &z[1]);
        assert!(rx.end(7, 0).is_none());
        assert!(rx.cur.is_some());
        let mut b = feed_all(&mut rx, 8, &z).expect("收齐");
        assert_eq!(b.format, CF_DIBV5);
        assert!(bytes(&mut b) == raw);
    }

    #[test]
    fn clip_stream_rejects_and_frees() {
        use crate::clip::CF_DIB;
        let raw = test_dib(100_000);
        let z = deflate(&raw);
        let all = z.concat();
        let n = raw.len() as u32;
        let mut rx = ClipRx::default();
        // 偏移错位：立刻释放
        assert!(rx.start(9, CF_DIB, n, 1 << 20).is_ok());
        rx.chunk(9, 5, &all[..10]);
        assert!(rx.cur.is_none());
        // raw_len 声称比实际小：解压超长 → 立刻释放（不等结束）
        assert!(rx.start(10, CF_DIB, n - 1, 1 << 20).is_ok());
        rx.chunk(10, 0, &all);
        assert!(rx.cur.is_none());
        // raw_len 声称比实际大：结束时长度不符
        assert!(rx.start(12, CF_DIB, n + 1, 1 << 20).is_ok());
        assert!(feed_all(&mut rx, 12, &z).is_none());
        assert!(rx.cur.is_none());
        // 结束帧的总长不符 / 截断
        assert!(rx.start(13, CF_DIB, n, 1 << 20).is_ok());
        rx.chunk(13, 0, &all);
        assert!(rx.end(13, all.len() as u32 + 1).is_none());
        assert!(rx.start(14, CF_DIB, n, 1 << 20).is_ok());
        rx.chunk(14, 0, &all[..all.len() - 20]);
        assert!(rx.cur.is_some());
        assert!(rx.end(14, all.len() as u32 - 20).is_none());
        // 垃圾数据
        assert!(rx.start(11, CF_DIB, 100, 1 << 20).is_ok());
        rx.chunk(11, 0, &[0xff; 64]);
        assert!(rx.cur.is_none());
        // 图片头不对：收到头就丢弃，不等收完
        let mut bad = raw.clone();
        bad[12] = 2; // planes
        let zb = deflate(&bad).concat();
        assert!(rx.start(15, CF_DIB, n, 1 << 20).is_ok());
        rx.chunk(15, 0, &zb[..zb.len().min(4096)]);
        assert!(rx.cur.is_none());
        // 对方作废
        assert!(rx.start(16, CF_DIB, n, 1 << 20).is_ok());
        rx.chunk(16, 0, &all[..10]);
        rx.abort(15);
        assert!(rx.cur.is_some());
        rx.abort(16);
        assert!(rx.cur.is_none());
        // 压缩数据长得离谱（不出数据的空块拖时间）
        assert!(rx.start(17, CF_DIB, 10, 1 << 20).is_ok());
        let max = rx.cur.as_ref().unwrap().max_compressed();
        rx.chunk(17, 0, &vec![0u8; max + 1]);
        assert!(rx.cur.is_none());
    }

    #[test]
    fn clip_cancel_and_stall() {
        let raw = test_dib(50_000);
        let z = deflate(&raw).concat();
        let mut rx = ClipRx::default();
        // 取消：之后这个 id 的分片都忽略
        assert!(rx.start(3, 8, raw.len() as u32, 1 << 20).is_ok());
        rx.chunk(3, 0, &z[..10]);
        rx.reset();
        rx.chunk(3, 10, &z[10..]);
        assert!(rx.end(3, z.len() as u32).is_none());
        assert!(rx.cur.is_none());
        // 30 秒没进展：释放
        assert!(rx.start(4, 8, raw.len() as u32, 1 << 20).is_ok());
        rx.expire(Instant::now() + Duration::from_secs(20));
        assert!(rx.cur.is_some());
        rx.expire(Instant::now() + CLIP_STALL + Duration::from_secs(1));
        assert!(rx.cur.is_none());
    }

    #[test]
    fn clip_limits() {
        let mut c = ClipCfg { enabled: true, images: false, max_text_kb: 1, max_image_mb: 1, notify: true };
        assert_eq!(clip_limit(&c, 13), Some(2050));
        assert_eq!(clip_limit(&c, 8), None);
        c.images = true;
        assert_eq!(clip_limit(&c, 17), Some(1 << 20));
        c.enabled = false;
        assert_eq!(clip_limit(&c, 13), None);
        assert_eq!(clip_limit(&c, 8), None);
        // 大段文字：解进末尾带 0 的内存，可直接当 CF_UNICODETEXT；摘要、UTF-8 长度与整段算的一致
        let text = "大段文字😀".repeat(100);
        let raw = crate::clip::utf16le(&text);
        let mut rx = ClipRx::default();
        assert!(rx.start(1, 13, raw.len() as u32, 1 << 20).is_ok());
        let mut b = feed_all(&mut rx, 1, &deflate(&raw)).expect("收齐");
        assert_eq!(b.format, 13);
        assert_eq!(b.text_len, text.len());
        assert_eq!(b.hash, crate::clip::hash_text(&text));
        let m = bytes(&mut b);
        assert_eq!(m.len(), raw.len() + 2);
        assert_eq!(&m[..raw.len()], &raw[..]);
        assert_eq!(&m[raw.len()..], &[0, 0]);
    }

    // ------------------------------------------------ 两个进程内的“对等端”

    fn free_port() -> u16 {
        loop {
            let u = UdpSocket::bind("127.0.0.1:0").unwrap();
            let p = u.local_addr().unwrap().port();
            if TcpListener::bind(("127.0.0.1", p)).is_ok() {
                return p;
            }
        }
    }

    struct Node {
        net: Arc<Net>,
        hub: Arc<Hub>,
        input: Receiver<ToInput>,
        desktop: Receiver<ToDesktop>,
    }

    fn node(name: &str, port: u16, peer: String, key: &str, layout: Layout, rev: u64) -> Node {
        // 同一进程里的两个节点要有不同的 id，否则会被当成“连到自己”
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        node_id(name, port, peer, key, layout, rev, u64::from_le_bytes(b) | 1)
    }

    fn node_id(name: &str, port: u16, peer: String, key: &str, layout: Layout, rev: u64, id: u64) -> Node {
        let mut cfg = Config { name: name.into(), port, peer, key: key.into(), ..Default::default() };
        cfg.layout.layout = layout;
        cfg.layout.rev = rev;
        let (hub, HubRx { input, desktop, net }) = Hub::new(cfg);
        let hub = Arc::new(hub);
        let injector = Arc::new(Injector::new(hub.clone()));
        let opts = Opts { bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST), persist: false, dial_every: Duration::from_millis(100), id, audio: None };
        let net = start(hub.clone(), net, injector, opts);
        Node { net, hub, input, desktop }
    }

    /// 两个连好的节点（B 主动连 A）。
    fn pair() -> (Node, Node) {
        let key = config::new_key_hex();
        let (pa, pb) = (free_port(), free_port());
        let a = node("A", pa, String::new(), &key, Layout::default(), 0);
        let b = node("B", pb, format!("127.0.0.1:{pa}"), &key, Layout::default(), 0);
        wait("双方连上", 10, || a.hub.status().connected && b.hub.status().connected);
        (a, b)
    }

    fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("超时：{what}");
    }

    /// 像压缩工作线程那样：开始一条流（未发出的至多 1 MiB），边压缩边交分片。线程结果 = 是否送完。
    fn send_stream(hub: &Arc<Hub>, format: u32, raw: Vec<u8>) -> std::thread::JoinHandle<bool> {
        let (tx, rx) = crate::hub::clip_stream(1 << 20);
        hub.to_net(Out::ClipStream { format, raw_len: raw.len() as u32, data: rx });
        std::thread::spawn(move || {
            let mut z = proto::Deflater::new();
            let mut ok = true;
            let mut it = raw.chunks(64 * 1024).peekable();
            while ok && let Some(b) = it.next() {
                ok = z.write(b, it.peek().is_none(), &mut |p| tx.send(p, &|| true));
            }
            if ok {
                tx.finish();
            }
            ok
        })
    }

    fn next_desktop(rx: &Receiver<ToDesktop>, secs: u64) -> ToDesktop {
        next_desktop_opt(rx, secs).expect("desktop 消息超时")
    }

    fn next_desktop_opt(rx: &Receiver<ToDesktop>, secs: u64) -> Option<ToDesktop> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(ToDesktop::Status | ToDesktop::ClipForget) => continue, // 状态刷新、“已同步”记账：不是测试要看的内容
                Ok(m) => return Some(m),
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn two_peers_end_to_end() {
        let key = config::new_key_hex();
        let (pa, pb) = (free_port(), free_port());
        let a = node("A", pa, String::new(), &key, Layout::default(), 0);
        let b_layout = Layout { side: Side::Left, offset: 100, scale: 1.0 };
        let b = node("B", pb, format!("127.0.0.1:{pa}"), &key, b_layout, 5);

        wait("双方连上", 10, || a.hub.status().connected && b.hub.status().connected);
        assert_eq!(a.hub.status().peer_name, "B");
        assert_eq!(b.hub.status().peer_name, "A");

        // A 收到 PeerUp(B) 和镜像后的排布
        let mut got_up = false;
        let mut got_cfg = false;
        let end = Instant::now() + Duration::from_secs(5);
        while !(got_up && got_cfg) && Instant::now() < end {
            match a.input.recv_timeout(Duration::from_millis(200)) {
                Ok(ToInput::PeerUp { name, .. }) => {
                    assert_eq!(name, "B");
                    got_up = true;
                }
                Ok(ToInput::Config(c)) => {
                    assert_eq!(c.layout.rev, 5);
                    assert_eq!(c.layout.layout, b_layout.mirrored());
                    got_cfg = true;
                }
                _ => {}
            }
        }
        assert!(got_up && got_cfg);
        assert_eq!(a.hub.cfg().layout.layout, Layout { side: Side::Right, offset: -100, scale: 1.0 });
        // B 的 rev 不比 A 旧 → 不变
        assert_eq!(b.hub.cfg().layout.layout, b_layout);

        // 文字
        a.hub.send(Msg::ClipText { text: "你好 kmlink".into() });
        match next_desktop(&b.desktop, 5) {
            ToDesktop::ClipText(t) => assert_eq!(t, "你好 kmlink"),
            m => panic!("{m:?}"),
        }

        // 图片：多片流式，中间夹着文字
        let raw = test_dib(300_000);
        assert!(deflate(&raw).len() > 1);
        // 多片图片完整到达；期间插进来的控制消息（Yield）不用等图片发完
        let t = send_stream(&b.hub, 17, raw.clone());
        b.hub.send(Msg::Yield { epoch: 0 });
        wait("Yield 先到", 10, || matches!(a.input.try_recv(), Ok(ToInput::Yield(_))));
        match next_desktop(&a.desktop, 10) {
            ToDesktop::ClipData(mut d) => {
                assert_eq!(d.format, 17);
                assert_eq!(d.hash, crate::clip::hash_image(17, &raw));
                assert!(bytes(&mut d) == raw);
            }
            m => panic!("{m:?}"),
        }
        assert!(t.join().unwrap());

        // 大段文字走同一条路
        let text = "大段文字 abc\n".repeat(20_000);
        let t = send_stream(&b.hub, 13, crate::clip::utf16le(&text));
        match next_desktop(&a.desktop, 10) {
            ToDesktop::ClipData(d) => {
                assert_eq!((d.format, d.text_len, d.hash), (13, text.len(), crate::clip::hash_text(&text)));
            }
            m => panic!("{m:?}"),
        }
        assert!(t.join().unwrap());

        // 图片还没发完时又复制了文字：文字是最新内容，对方最后拿到的必须是文字（旧图片作废）
        let t = send_stream(&b.hub, 17, raw.clone());
        b.hub.send(Msg::ClipText { text: "插队".into() });
        loop {
            match next_desktop(&a.desktop, 10) {
                ToDesktop::ClipText(t) => {
                    assert_eq!(t, "插队");
                    break;
                }
                ToDesktop::ClipData(_) => continue, // 若图片抢在文字之前发完也可以，但不能在文字之后
                m => panic!("{m:?}"),
            }
        }
        assert!(next_desktop_opt(&a.desktop, 1).is_none(), "文字之后不应再收到旧图片");
        let _ = t.join().unwrap(); // 被顶掉时压缩线程放弃（false），已发完则 true

        // UDP：双方通过 Hi 学到对方地址
        let learned = |n: &Node| n.net.current().is_some_and(|s| lock(&s.udp_peer).learned);
        wait("UDP 打通", 5, || learned(&a) && learned(&b));
        let sa = a.net.current().unwrap();
        assert_eq!(lock(&sa.udp_peer).addr.unwrap().port(), pb);

        // UDP 通了以后鼠标位置只走 UDP（含补发）
        b.hub.to_net(Out::Move { epoch: 1, seq: 1, x: 5, y: 5 });
        wait("UDP Move 到达", 5, || a.net.stats.udp_moves.load(Ordering::Relaxed) >= 2);
        assert_eq!(a.net.stats.tcp_moves.load(Ordering::Relaxed), 0);

        // Ping/Pong 保持连接，会话不变
        let id_before = sa.id;
        std::thread::sleep(Duration::from_millis(1500));
        assert!(a.hub.status().connected && b.hub.status().connected);
        assert_eq!(a.net.current().unwrap().id, id_before);
    }

    #[test]
    fn mouse_moves_not_blocked_by_image() {
        // 传大图时（TCP 写路径被占着），UDP 鼠标位置照样立刻到
        let (a, b) = pair();
        let learned = |n: &Node| n.net.current().is_some_and(|s| lock(&s.udp_peer).learned);
        wait("UDP 打通", 5, || learned(&a) && learned(&b));
        let sb = b.net.current().unwrap();
        let raw = test_dib(4_000_000);
        let u0 = a.net.stats.udp_moves.load(Ordering::Relaxed);
        let t;
        {
            // 卡住 B 的 TCP 发送（模拟 WiFi 上分片排不出去）
            let _g = lock(&sb.tx);
            t = send_stream(&b.hub, 8, raw.clone());
            std::thread::sleep(Duration::from_millis(50));
            b.hub.to_net(Out::Move { epoch: 1, seq: 1, x: 1, y: 1 });
            wait("TCP 卡着时 UDP Move 照样到", 2, || a.net.stats.udp_moves.load(Ordering::Relaxed) > u0);
            // 压缩线程填满 1 MiB 后等着（背压），不会把整张图压完堆在内存里
            std::thread::sleep(Duration::from_millis(300));
            assert!(!t.is_finished());
        }
        // 放开后照常发完
        match next_desktop(&a.desktop, 20) {
            ToDesktop::ClipData(mut d) => assert!(bytes(&mut d) == raw),
            m => panic!("{m:?}"),
        }
        assert!(t.join().unwrap());
    }

    #[test]
    fn stream_stops_when_session_drops() {
        // 传图时会话断了：压缩线程不会死等，立刻放弃
        let (_a, b) = pair();
        let sb = b.net.current().unwrap();
        let raw = test_dib(8_000_000);
        let t;
        {
            let _g = lock(&sb.tx);
            t = send_stream(&b.hub, 8, raw);
            std::thread::sleep(Duration::from_millis(300));
            assert!(!t.is_finished(), "应在等 net 发走");
            sb.close();
        }
        let t0 = Instant::now();
        assert!(!t.join().unwrap());
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn udp_stall_falls_back_to_tcp() {
        let (a, b) = pair();
        let learned = |n: &Node| n.net.current().is_some_and(|s| lock(&s.udp_peer).learned);
        wait("UDP 打通", 5, || learned(&a) && learned(&b));
        let sa = a.net.current().unwrap();
        let tcp0 = b.net.stats.tcp_moves.load(Ordering::Relaxed);
        {
            // A 的 UDP 接收卡住：A 收不到 B 的 UDP 包
            let _g = lock(&sa.udp_rx);
            wait("A 发现 UDP 不通", 5, || !sa.udp_live());
            a.hub.to_net(Out::Move { epoch: 1, seq: 1, x: 5, y: 5 });
            wait("Move 改走 TCP", 5, || b.net.stats.tcp_moves.load(Ordering::Relaxed) > tcp0);
        }
        // UDP 恢复：切回只走 UDP
        wait("A 的 UDP 恢复", 5, || sa.udp_live());
        std::thread::sleep(Duration::from_millis(300));
        let (tcp1, udp1) = (b.net.stats.tcp_moves.load(Ordering::Relaxed), b.net.stats.udp_moves.load(Ordering::Relaxed));
        a.hub.to_net(Out::Move { epoch: 1, seq: 2, x: 6, y: 6 });
        wait("UDP Move 到达", 5, || b.net.stats.udp_moves.load(Ordering::Relaxed) > udp1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(b.net.stats.tcp_moves.load(Ordering::Relaxed), tcp1);
    }

    #[test]
    fn quiet_peer_releases_keys_but_keeps_session() {
        let (a, b) = pair();
        let sb = b.net.current().unwrap();
        let id = a.net.current().unwrap().id;
        let s0 = a.net.stats.stales.load(Ordering::Relaxed);
        let t0 = Instant::now();
        {
            // B 发不出任何 TCP 帧（Ping 也发不出）
            let _g = lock(&sb.tx);
            wait("A 抬起按键", 4, || a.net.stats.stales.load(Ordering::Relaxed) > s0);
        }
        assert!(t0.elapsed() >= Duration::from_millis(1000));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(a.net.current().unwrap().id, id, "会话不应断开");
        // 每段安静只抬一次
        assert_eq!(a.net.stats.stales.load(Ordering::Relaxed), s0 + 1);
    }

    #[test]
    fn cancel_clip_drops_incoming_image() {
        let (a, b) = pair();
        let sb = b.net.current().unwrap();
        let raw = test_dib(100_000);
        let z = deflate(&raw).concat();
        assert!(z.len() > CHUNK);
        let hdr = |id: u32| proto::encode_msg(&Msg::ClipStart { id, format: 8, raw_len: raw.len() as u32 });
        // B 发了开始帧和第一片
        sb.send_plain(&hdr(777)).unwrap();
        sb.send_plain(&proto::encode_chunk(777, 0, &z[..CHUNK])).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        // A 本机剪贴板被更新的内容取代
        a.hub.to_net(Out::CancelClip);
        wait("A 处理取消", 5, || a.net.clip_gen.load(Ordering::Relaxed) == 1);
        sb.send_plain(&proto::encode_chunk(777, CHUNK as u32, &z[CHUNK..])).unwrap();
        sb.send_plain(&proto::encode_end(777, z.len() as u32)).unwrap();
        assert!(next_desktop_opt(&a.desktop, 1).is_none(), "取消后不应再拼出这张图");
        // 对方作废的流也拼不出来
        sb.send_plain(&hdr(778)).unwrap();
        sb.send_plain(&proto::encode_chunk(778, 0, &z[..CHUNK])).unwrap();
        sb.send_plain(&proto::encode_abort(778)).unwrap();
        sb.send_plain(&proto::encode_chunk(778, CHUNK as u32, &z[CHUNK..])).unwrap();
        sb.send_plain(&proto::encode_end(778, z.len() as u32)).unwrap();
        assert!(next_desktop_opt(&a.desktop, 1).is_none(), "作废后不应再拼出这张图");
        // 之后新的传输照常
        sb.send_plain(&hdr(779)).unwrap();
        sb.send_plain(&proto::encode_chunk(779, 0, &z)).unwrap();
        sb.send_plain(&proto::encode_end(779, z.len() as u32)).unwrap();
        match next_desktop(&a.desktop, 5) {
            ToDesktop::ClipData(mut d) => {
                assert_eq!(d.format, 8);
                assert!(bytes(&mut d) == raw);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn wrong_key_does_not_connect() {
        let (pa, pb) = (free_port(), free_port());
        let a = node("A", pa, String::new(), &config::new_key_hex(), Layout::default(), 0);
        let b = node("B", pb, format!("127.0.0.1:{pa}"), &config::new_key_hex(), Layout::default(), 0);
        wait("报告密钥不一致", 10, || b.hub.status().last_error.contains("密钥") && a.hub.status().last_error.contains("密钥"));
        assert!(!a.hub.status().connected && !b.hub.status().connected);
        assert!(a.net.current().is_none() && b.net.current().is_none());
    }

    #[test]
    fn invalid_key_never_dials() {
        let (pa, pb) = (free_port(), free_port());
        let a = node("A", pa, String::new(), &config::new_key_hex(), Layout::default(), 0);
        let b = node("B", pb, format!("127.0.0.1:{pa}"), "not-a-key", Layout::default(), 0);
        wait("报告密钥无效", 5, || b.hub.status().last_error == "密钥无效");
        std::thread::sleep(Duration::from_millis(500));
        assert!(a.net.current().is_none());
        assert!(a.hub.status().last_error.is_empty());
    }

    #[test]
    fn handshake_deadline_is_absolute() {
        let (mut c, mut s) = tcp_pair();
        // 慢速连接：每 50ms 一个字节（魔数对，后面是 0），单次读永远不会超时
        let t = std::thread::spawn(move || {
            for i in 0..40 {
                if c.write_all(&[MAGIC.get(i).copied().unwrap_or(0)]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let t0 = Instant::now();
        let mut dl = Deadline { s: &mut s, until: Instant::now() + Duration::from_millis(300) };
        let e = handshake(&mut dl, &psk(), Role::Server(&no_reflect)).err().expect("应超时");
        assert!(matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock), "{e:?}");
        assert!(t0.elapsed() < Duration::from_millis(1000));
        drop(s);
        let _ = t.join();
    }

    #[test]
    fn first_frame_cap_and_auth_fail_kind() {
        let (mut c, mut s) = tcp_pair();
        c.write_all(&((MAX_FIRST_FRAME as u32) + 1).to_le_bytes()).unwrap();
        let e = read_frame(&mut s, &mut Opener::new(&psk()), MAX_FIRST_FRAME).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(!is_auth_fail(&e));
        let (mut c, mut cc, mut s, mut sc) = hs_pair(psk(), [0x43; 32]);
        write_frame(&mut c, &mut cc.tx, &proto::encode_msg(&Msg::Yield { epoch: 0 })).unwrap();
        assert!(is_auth_fail(&read_frame(&mut s, &mut sc.rx, MAX_FIRST_FRAME).unwrap_err()));
    }

    #[test]
    fn pure_rules() {
        // 鼠标走哪条路
        assert_eq!(move_route(true, true), (true, false));
        assert_eq!(move_route(true, false), (true, true));
        assert_eq!(move_route(false, false), (false, true));
        assert_eq!(move_route(false, true), (false, true));
        // 补发时机
        let t = Instant::now();
        let l = LastMove { sid: 1, mv: UdpMsg::Hi, at: t, repaired: false };
        assert!(!l.due(t + Duration::from_millis(50), false));
        assert!(l.due(t + REPAIR_AFTER, false));
        assert!(l.due(t + Duration::from_millis(10), true));
        assert!(!l.due(t + REPAIR_FOR, true));
        let r = LastMove { repaired: true, ..l };
        assert!(!r.due(t + Duration::from_millis(500), false));
        assert!(r.due(t + Duration::from_millis(2500), true));
        // 排布：rev 更新才采用；rev 相同只有 id 小的一方采用
        let x = Layout::default();
        let y = Layout { side: Side::Left, offset: 10, scale: 1.0 };
        assert!(adopt_layout(1, &x, &y, 2, 5, 9));
        assert!(!adopt_layout(2, &x, &y, 2, 9, 5));
        assert!(adopt_layout(2, &x, &y, 2, 5, 9));
        assert!(!adopt_layout(2, &x, &x, 2, 5, 9));
        assert!(!adopt_layout(2, &x, &y, 2, 5, 0));
        assert!(!adopt_layout(3, &x, &y, 2, 5, 9));
        // 排布合理性：rev 超前 1 天以上、数值离谱的不要
        let now = 1_700_000_000_000u64;
        assert!(layout_sane(&y, now + 1000, now));
        assert!(layout_sane(&y, now + MAX_REV_AHEAD, now));
        assert!(!layout_sane(&y, now + MAX_REV_AHEAD + 1, now));
        assert!(!layout_sane(&y, u64::MAX, now));
        assert!(!layout_sane(&Layout { offset: i32::MIN, ..y }, now, now));
        assert!(!layout_sane(&Layout { scale: f64::NAN, ..y }, now, now));
        assert!(!layout_sane(&Layout { scale: 0.0, ..y }, now, now));
        // 自连后 60 秒内不再拨同一个 peer；peer 变了或到期就解除
        let mut slot = Some(("1.2.3.4".to_string(), t + SELF_BACKOFF));
        assert!(self_blocked(&mut slot, "1.2.3.4", t));
        assert!(!self_blocked(&mut slot, "1.2.3.4", t + SELF_BACKOFF));
        assert!(slot.is_none());
        let mut slot = Some(("1.2.3.4".to_string(), t + SELF_BACKOFF));
        assert!(!self_blocked(&mut slot, "1.2.3.5", t));
        assert!(slot.is_none());
        // 认证前的错误何时显示在状态里
        assert!(show_notice(false, true, false));
        assert!(show_notice(false, false, true));
        assert!(!show_notice(false, false, false)); // 填了对方地址，陌生 IP 不能改状态
        assert!(!show_notice(true, true, true)); // 已有会话时不改
        assert_eq!(peer_literal_ip(" 10.0.0.2:5 "), Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))));
        assert_eq!(peer_literal_ip("10.0.0.2"), Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))));
        assert!(peer_literal_ip("[::1]").is_some());
        assert_eq!(peer_literal_ip("my-pc"), None);
        // 另一台同密钥的电脑：旧会话活跃时拒绝
        assert!(other_peer_busy(1, 2, Duration::from_millis(500)));
        assert!(!other_peer_busy(1, 2, Duration::from_secs(3)));
        assert!(!other_peer_busy(1, 1, Duration::from_millis(500)));
        assert!(!other_peer_busy(0, 2, Duration::from_millis(500)));
        // UDP 单包错误不退避
        assert!(per_datagram_err(&io::Error::from_raw_os_error(10040)));
        assert!(per_datagram_err(&io::Error::from(io::ErrorKind::ConnectionReset)));
        assert!(!per_datagram_err(&io::Error::from_raw_os_error(10038)));
        // 设置签名：只看密钥/对方地址/端口
        let mut c = Config { key: config::new_key_hex(), peer: " 10.0.0.2 ".into(), ..Default::default() };
        let sig = CfgSig::of(&c);
        c.peer = "10.0.0.2".into();
        c.name = "别的名字".into();
        assert_eq!(CfgSig::of(&c), sig);
        c.port += 1;
        assert_ne!(CfgSig::of(&c), sig);
    }

    #[test]
    fn resolve_peer_literals() {
        let v = |p: &str| resolve_peer(p, 47900).unwrap();
        let sa = |s: &str| vec![s.parse::<SocketAddr>().unwrap()];
        assert_eq!(v("10.0.0.2"), sa("10.0.0.2:47900"));
        assert_eq!(v(" 10.0.0.2:5 "), sa("10.0.0.2:5"));
        assert_eq!(v("fe80::1"), sa("[fe80::1]:47900"));
        assert_eq!(v("[::1]"), sa("[::1]:47900"));
        assert_eq!(v("[::1]:9"), sa("[::1]:9"));
        assert!(resolve_peer("host:notaport", 1).is_err());
    }

    #[test]
    fn self_connect_detected() {
        let key = config::new_key_hex();
        let p = free_port();
        let peer = format!("127.0.0.1:{p}");
        let a = node("A", p, peer.clone(), &key, Layout::default(), 0);
        wait("报告连到自己", 10, || a.hub.status().last_error == ERR_SELF);
        assert!(lock(&a.net.self_peer).as_ref().is_some_and(|(p, until)| *p == peer && *until > Instant::now()));
        // 认证前（握手阶段）就识别出来了
        let n = a.net.stats.reflect_rejects.load(Ordering::Relaxed);
        assert!(n >= 1);
        // 退避期间不再拨（测试里拨号间隔 100ms）
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(a.net.stats.reflect_rejects.load(Ordering::Relaxed), n);
        assert!(a.net.current().is_none());
        assert!(!a.hub.status().connected);
        assert!(!a.input.try_iter().any(|m| matches!(m, ToInput::PeerUp { .. })));
    }

    #[test]
    fn moves_fall_back_to_tcp_without_udp() {
        let key = config::new_key_hex();
        let (pa, pb) = (free_port(), free_port());
        // 占住 B 的 UDP 端口
        let _block = UdpSocket::bind(("127.0.0.1", pb)).unwrap();
        let a = node("A", pa, String::new(), &key, Layout::default(), 0);
        let b = node("B", pb, format!("127.0.0.1:{pa}"), &key, Layout::default(), 0);
        assert!(b.net.udp.get().is_none());
        wait("双方连上", 10, || a.hub.status().connected && b.hub.status().connected);
        wait("B 报 UDP 不可用", 5, || b.hub.status().last_error == ERR_UDP_DOWN);
        // A → B：走 TCP，最后一个还会补发
        a.hub.to_net(Out::Move { epoch: 1, seq: 1, x: 5, y: 5 });
        wait("TCP Move 到达并补发", 5, || b.net.stats.tcp_moves.load(Ordering::Relaxed) >= 2);
        // A 一直收不到 B 的 UDP：宽限期后也报
        wait("A 报 UDP 不通", 6, || a.hub.status().last_error == ERR_UDP_DOWN);
        assert!(a.hub.status().connected);
    }

    #[test]
    fn config_change_restarts_session() {
        let key = config::new_key_hex();
        let (pa, pb) = (free_port(), free_port());
        let a = node("A", pa, String::new(), &key, Layout::default(), 0);
        let b = node("B", pb, format!("127.0.0.1:{pa}"), &key, Layout::default(), 0);
        wait("双方连上", 10, || a.net.current().is_some() && b.net.current().is_some());
        let old = a.net.current().unwrap().id;
        a.hub.cfg.write().unwrap().peer = format!("127.0.0.1:{pb}");
        wait("A 用新设置重连", 10, || a.net.current().is_some_and(|s| s.id != old) && a.hub.status().connected);
    }
}
