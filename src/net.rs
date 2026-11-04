//! 网络：一个会话 = 一条 TCP（可靠、加密、分帧）+ 一个 UDP 端口（鼠标位置）。
//!
//! 线程：
//! - listener：TcpListener 绑 0.0.0.0:cfg.port；dialer：cfg.peer 有值且当前没有会话时，每 2 秒（加 0~500ms 抖动）连一次，超时 2 秒。
//! - 每个会话：reader 线程（读帧、分发）+ writer 线程（从 hub 的 net 通道取 Out，可靠消息优先，剪贴板分片低优先级穿插）。
//! - udp：UdpSocket 绑 0.0.0.0:cfg.port，一个接收线程；发送在 writer 里做（Out::Move 只发 channel 里最新的那个）。
//!
//! 握手见 crypto.rs。之后双方先发 Hello（sys::monitors()、cfg.name、udp_port = cfg.port），再发 Layout（本机配置里的排布与 rev）。
//! TCP：set_nodelay(true)。读超时 6 秒；每 1 秒发 Ping；6 秒收不到任何帧就断开。
//! 同时只保留一个会话：新会话认证成功时，若旧会话不到 2 秒，保留 nonce_c 字典序更大的那个（两边会选同一个），否则用新的替换旧的。
//!
//! 收到的消息：
//!   Hello → config::save_peer_desk；hub.to_input(PeerUp{name, desk})；hub.set_status(connected=true, peer_name, peer_addr)
//!   Layout{rev 比本机新} → 镜像（Layout::mirrored）后写回 hub.cfg 和 config.toml（rev 保持对方的值），再 hub.to_input(Config(..))
//!   Enter/Leave/Button/Wheel/Key → injector
//!   Yield → hub.to_input(Yield)
//!   ClipText → hub.to_desktop(ClipText)；ClipImage + 分片 → 拼齐后 miniz_oxide 解压（上限 raw_len ≤ max_image_mb）→ hub.to_desktop(ClipImage)
//!   Ping → 回 Pong
//! 会话结束：injector.release_all()；hub.to_input(PeerDown)；hub.set_status(connected=false)。
//! UDP：对方地址 = TCP 对端 IP + Hello 里的 udp_port；之后以最近一个认证通过的 UDP 包源地址为准。每秒发 UdpMsg::Hi。
//!   收到 Move → injector.moved。没有会话时丢弃所有 UDP。
//! 配置变化（desktop 线程重新加载后会 send(Msg::Layout)），net 只管转发。
//! 发送端 Out::Msg 在没有会话时直接丢弃（不排队），Out::ClipImage 同样丢弃。
//!
//! 实现说明：writer 是整个进程只有一个的常驻线程（独占 hub 的 net 通道），每次发送前看 `Net::holder` 里的当前会话；
//! 会话来来去去，writer 不变。reader 线程每个会话一个（就是握手的那个线程）。

use crate::config::{self, Config};
use crate::crypto::{self, Opener, Sealer, UdpOpener, UdpSealer};
use crate::geom::{Desk, Layout};
use crate::hub::{Hub, Out, ToDesktop, ToInput};
use crate::inject::Injector;
use crate::proto::{self, CHUNK, Frame, MAGIC, Msg, PROTO_VERSION, UdpMsg};
use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 帧（密文）长度上限。
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// 明文上限（密文 = 明文 + 16 字节 tag）。
const MAX_PLAIN: usize = MAX_FRAME - 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(6);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PING_EVERY: Duration = Duration::from_secs(1);
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
const TIE_WINDOW: Duration = Duration::from_secs(2);
/// 同时进行中的入站握手上限（防止局域网里有东西乱连把线程堆满）。
const MAX_PENDING_HANDSHAKES: usize = 8;

pub fn spawn(hub: Arc<Hub>, rx: Receiver<Out>, injector: Arc<Injector>) {
    let opts = Opts { bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED), persist: true, dial_every: Duration::from_secs(2) };
    start(hub, rx, injector, opts);
}

struct Opts {
    bind_ip: IpAddr,
    /// 是否写盘（config.toml / peer.json）。测试里关掉。
    persist: bool,
    dial_every: Duration,
}

type SessionHandle = Arc<Session>;

struct Net {
    hub: Arc<Hub>,
    injector: Arc<Injector>,
    opts: Opts,
    /// 当前会话（至多一个）。
    holder: Mutex<Option<SessionHandle>>,
    udp: Option<UdpSocket>,
    next_id: AtomicU64,
    pending: AtomicUsize,
    key_warned: AtomicBool,
    /// 最近一次记下的错误（相同错误不重复写日志）。
    last_err: Mutex<String>,
}

struct Session {
    id: u64,
    started: Instant,
    nonce_c: [u8; 32],
    peer: SocketAddr,
    /// 只用来 shutdown。
    ctl: TcpStream,
    tx: Mutex<TcpTx>,
    udp_tx: Mutex<UdpSealer>,
    udp_rx: Mutex<UdpOpener>,
    udp_peer: Mutex<UdpPeer>,
}

struct TcpTx {
    stream: TcpStream,
    sealer: Sealer,
}

#[derive(Default)]
struct UdpPeer {
    addr: Option<SocketAddr>,
    /// 已从认证通过的 UDP 包学到源地址（之后 Hello 不再覆盖）。
    learned: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        crate::log!("net：创建线程 {name} 失败：{e}");
    }
}

fn start(hub: Arc<Hub>, rx: Receiver<Out>, injector: Arc<Injector>, opts: Opts) -> Arc<Net> {
    let port = hub.cfg().port;
    let udp = match UdpSocket::bind((opts.bind_ip, port)) {
        Ok(s) => Some(s),
        Err(e) => {
            crate::log!("UDP 端口 {port} 绑定失败：{e}（鼠标移动将不可用）");
            hub.set_status(|s| s.last_error = format!("UDP 端口 {port} 被占用"));
            None
        }
    };
    let net = Arc::new(Net {
        hub,
        injector,
        opts,
        holder: Mutex::new(None),
        udp,
        next_id: AtomicU64::new(1),
        pending: AtomicUsize::new(0),
        key_warned: AtomicBool::new(false),
        last_err: Mutex::new(String::new()),
    });
    let n = net.clone();
    spawn_named("net-writer", move || n.run_writer(rx));
    let n = net.clone();
    spawn_named("net-listener", move || n.run_listener());
    let n = net.clone();
    spawn_named("net-dialer", move || n.run_dialer());
    if net.udp.is_some() {
        let n = net.clone();
        spawn_named("net-udp", move || n.run_udp());
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
}

fn read_hs<S: Read>(s: &mut S) -> io::Result<[u8; 32]> {
    let mut b = [0u8; 36];
    s.read_exact(&mut b)?;
    if &b[..4] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "握手魔数不对"));
    }
    let mut n = [0u8; 32];
    n.copy_from_slice(&b[4..]);
    Ok(n)
}

/// 明文握手 + 派生密钥。client 先发。
fn handshake<S: Read + Write>(s: &mut S, psk: &[u8; 32], client: bool) -> io::Result<Chans> {
    let mine = crypto::random32();
    let mut hello = [0u8; 36];
    hello[..4].copy_from_slice(MAGIC);
    hello[4..].copy_from_slice(&mine);
    let (nc, ns) = if client {
        s.write_all(&hello)?;
        (mine, read_hs(s)?)
    } else {
        let theirs = read_hs(s)?;
        s.write_all(&hello)?;
        (theirs, mine)
    };
    let k = crypto::derive(psk, &nc, &ns);
    Ok(if client {
        Chans { nonce_c: nc, tx: Sealer::new(&k.c2s), rx: Opener::new(&k.s2c), utx: UdpSealer::new(&k.udp_c2s), urx: UdpOpener::new(&k.udp_s2c) }
    } else {
        Chans { nonce_c: nc, tx: Sealer::new(&k.s2c), rx: Opener::new(&k.c2s), utx: UdpSealer::new(&k.udp_s2c), urx: UdpOpener::new(&k.udp_c2s) }
    })
}

fn write_frame<W: Write>(w: &mut W, sealer: &mut Sealer, plain: &[u8]) -> io::Result<()> {
    if plain.len() > MAX_PLAIN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "帧太大"));
    }
    let ct = sealer.seal(plain);
    let mut buf = Vec::with_capacity(4 + ct.len());
    buf.extend_from_slice(&(ct.len() as u32).to_le_bytes());
    buf.extend_from_slice(&ct);
    w.write_all(&buf)
}

/// 读一帧并解密。解不开（篡改/重放/密钥不同）返回 InvalidData。
fn read_frame<R: Read>(r: &mut R, opener: &mut Opener) -> io::Result<Vec<u8>> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb)?;
    let len = u32::from_le_bytes(lb) as usize;
    if !(16..=MAX_FRAME).contains(&len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("帧长度不合法：{len}")));
    }
    let mut ct = vec![0u8; len];
    r.read_exact(&mut ct)?;
    opener.open(&ct).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "解密失败"))
}

/// 新会话是否应让位给旧会话（tie-break）。
fn keep_old(old_age: Duration, old_nonce_c: &[u8; 32], new_nonce_c: &[u8; 32]) -> bool {
    old_age < TIE_WINDOW && old_nonce_c > new_nonce_c
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
    fn send_msg(&self, m: &Msg) -> io::Result<()> {
        self.send_plain(&proto::encode_msg(m))
    }
    fn close(&self) {
        let _ = self.ctl.shutdown(Shutdown::Both);
    }
    fn send_udp(&self, sock: &UdpSocket, m: &UdpMsg) {
        let Some(addr) = lock(&self.udp_peer).addr else { return };
        let pkt = lock(&self.udp_tx).seal(&m.encode());
        let _ = sock.send_to(&pkt, addr);
    }
}

fn layout_msg(cfg: &Config) -> Msg {
    let l = cfg.layout.layout;
    Msg::Layout { side: l.side, offset: l.offset, scale: l.scale, rev: cfg.layout.rev }
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
        let udp_port = self.udp.as_ref().and_then(|u| u.local_addr().ok()).map(|a| a.port()).unwrap_or(cfg.port);
        Msg::Hello { ver: PROTO_VERSION, name: cfg.name.clone(), monitors: crate::sys::monitors().monitors, udp_port, id: crate::proto::instance_id() }
    }

    /// 握手、互发 Hello/Layout、读对方 Hello（= 认证）、登记为当前会话。
    fn establish(&self, mut stream: TcpStream, client: bool) -> Option<(SessionHandle, TcpStream, Opener, Msg)> {
        let cfg = self.hub.cfg();
        let psk = self.psk(&cfg)?;
        let peer = stream.peer_addr().ok()?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

        let ch = match handshake(&mut stream, &psk, client) {
            Ok(c) => c,
            Err(e) => {
                if client {
                    self.note_err(format!("与 {peer} 握手失败：{e}"));
                } else {
                    crate::log!("{peer} 入站握手失败：{e}");
                }
                return None;
            }
        };
        let Chans { nonce_c, mut tx, mut rx, utx, urx } = ch;
        let sent = write_frame(&mut stream, &mut tx, &proto::encode_msg(&self.hello(&cfg)))
            .and_then(|_| write_frame(&mut stream, &mut tx, &proto::encode_msg(&layout_msg(&cfg))));
        if let Err(e) = sent {
            crate::log!("向 {peer} 发送 Hello 失败：{e}");
            return None;
        }
        let first = match read_frame(&mut stream, &mut rx) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                self.note_err(format!("与 {peer} 认证失败：两边的配对密钥不一致"));
                return None;
            }
            Err(e) => {
                crate::log!("等待 {peer} 的 Hello 失败：{e}");
                return None;
            }
        };
        let hello = match proto::decode_frame(&first) {
            Some(Frame::Msg(m @ Msg::Hello { .. })) => m,
            _ => {
                crate::log!("{peer} 的第一帧不是 Hello，断开");
                return None;
            }
        };
        if let Msg::Hello { ver, udp_port, .. } = &hello {
            if *ver != PROTO_VERSION {
                self.note_err(format!("对方协议版本 {ver} 与本机 {PROTO_VERSION} 不同，请两边升级到同一版本"));
                return None;
            }
            let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
            let ctl = stream.try_clone().ok()?;
            let wstream = stream.try_clone().ok()?;
            let sess = Arc::new(Session {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                started: Instant::now(),
                nonce_c,
                peer,
                ctl,
                tx: Mutex::new(TcpTx { stream: wstream, sealer: tx }),
                udp_tx: Mutex::new(utx),
                udp_rx: Mutex::new(urx),
                udp_peer: Mutex::new(UdpPeer { addr: Some(SocketAddr::new(peer.ip(), *udp_port)), learned: false }),
            });
            if !self.install(&sess) {
                crate::log!("与 {peer} 的新连接让位给已有连接");
                sess.close();
                return None;
            }
            self.clear_err();
            crate::log!("已连接 {peer}（{}）", if client { "主动" } else { "被动" });
            return Some((sess, stream, rx, hello));
        }
        None
    }

    /// 登记新会话；按 tie-break 规则可能拒绝。
    fn install(&self, new: &SessionHandle) -> bool {
        let replaced = {
            let mut cur = lock(&self.holder);
            if let Some(old) = cur.as_ref() {
                if keep_old(old.started.elapsed(), &old.nonce_c, &new.nonce_c) {
                    return false;
                }
                old.close();
            }
            cur.replace(new.clone()).is_some()
        };
        self.injector.begin_session(new.id);
        if replaced {
            crate::log!("新连接替换旧连接");
            self.hub.to_input(ToInput::PeerDown);
        }
        true
    }

    fn end_session(&self, sess: &Session, why: &str) {
        sess.close();
        let was_current = {
            let mut cur = lock(&self.holder);
            if cur.as_ref().is_some_and(|c| c.id == sess.id) {
                *cur = None;
                true
            } else {
                false
            }
        };
        if was_current {
            crate::log!("与 {} 的连接断开：{why}", sess.peer);
            self.injector.end_session(sess.id);
            self.hub.to_input(ToInput::PeerDown);
            self.hub.set_status(|s| {
                s.connected = false;
                s.peer_addr.clear();
            });
        }
    }

    fn run_reader(&self, sess: SessionHandle, mut stream: TcpStream, mut opener: Opener, first: Msg) {
        let mut clip = ClipRx::default();
        self.handle(&sess, first, &mut clip);
        let why = loop {
            let plain = match read_frame(&mut stream, &mut opener) {
                Ok(p) => p,
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                    break "超过 6 秒没有收到数据".to_string();
                }
                Err(e) => break e.to_string(),
            };
            if !self.is_current(&sess) {
                break "已被新连接替换".to_string();
            }
            match proto::decode_frame(&plain) {
                Some(Frame::Msg(m)) => self.handle(&sess, m, &mut clip),
                Some(Frame::Chunk { id, offset, data }) => {
                    if let Some((format, data)) = clip.chunk(id, offset, &data) {
                        self.hub.to_desktop(ToDesktop::ClipImage { format, data });
                    }
                }
                None => crate::log!("忽略无法解析的帧（{} 字节）", plain.len()),
            }
        };
        self.end_session(&sess, &why);
    }

    fn handle(&self, sess: &Session, m: Msg, clip: &mut ClipRx) {
        match m {
            Msg::Hello { ver, name, monitors, udp_port, .. } => {
                if ver != PROTO_VERSION {
                    crate::log!("对方协议版本 {ver} 与本机 {PROTO_VERSION} 不同");
                }
                let desk = Desk::new(monitors);
                if self.opts.persist {
                    config::save_peer_desk(&name, &desk);
                }
                {
                    let mut u = lock(&sess.udp_peer);
                    if !u.learned {
                        u.addr = Some(SocketAddr::new(sess.peer.ip(), udp_port));
                    }
                }
                let addr = sess.peer.to_string();
                let n = name.clone();
                self.hub.set_status(|s| {
                    s.connected = true;
                    s.peer_name = n;
                    s.peer_addr = addr;
                    s.last_error.clear();
                });
                self.hub.to_input(ToInput::PeerUp { name, desk });
            }
            Msg::Layout { side, offset, scale, rev } => self.apply_layout(Layout { side, offset, scale }, rev),
            Msg::Enter { epoch, x, y } => self.injector.enter(sess.id, epoch, x, y),
            Msg::Leave { epoch } => self.injector.leave(sess.id, epoch),
            Msg::Button { epoch, x, y, button, down } => self.injector.button(sess.id, epoch, x, y, button, down),
            Msg::Wheel { epoch, x, y, delta, horizontal } => self.injector.wheel(sess.id, epoch, x, y, delta, horizontal),
            Msg::Key { epoch, scan, ext, vk, down } => self.injector.key(sess.id, epoch, scan, ext, vk, down),
            Msg::Move { epoch, seq, x, y } => self.injector.moved(sess.id, epoch, seq, x, y),
            Msg::Yield => self.hub.to_input(ToInput::Yield),
            Msg::ClipText { text } => {
                let c = self.hub.cfg().clipboard;
                if c.enabled && text.len() <= c.max_text_kb as usize * 1024 {
                    self.hub.to_desktop(ToDesktop::ClipText(text));
                }
            }
            Msg::ClipImage { id, format, total, raw_len } => {
                let c = self.hub.cfg().clipboard;
                if c.enabled && c.images {
                    if !clip.start(id, format, total, raw_len, c.max_image_mb as usize * 1024 * 1024) {
                        crate::log!("忽略对方的剪贴板图片（{total} / {raw_len} 字节，超过上限）");
                    }
                } else {
                    clip.reset();
                }
            }
            Msg::Ping { t } => {
                let _ = sess.send_msg(&Msg::Pong { t });
            }
            Msg::Pong { .. } => {}
        }
    }

    /// 对方的排布（对方视角）：rev 更新则镜像后保存。
    fn apply_layout(&self, theirs: Layout, rev: u64) {
        let new_cfg = {
            let mut c = self.hub.cfg.write().unwrap_or_else(|e| e.into_inner());
            if rev <= c.layout.rev {
                return;
            }
            c.layout.layout = theirs.mirrored();
            c.layout.rev = rev;
            c.clone()
        };
        crate::log!("采用对方的排布（rev {rev}）：对方在本机{}，错开 {}", new_cfg.layout.layout.side.label_zh(), new_cfg.layout.layout.offset);
        if self.opts.persist {
            if let Err(e) = new_cfg.save() {
                crate::log!("保存配置失败：{e}");
            }
        }
        self.hub.to_input(ToInput::Config(Box::new(new_cfg)));
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
            if self.pending.load(Ordering::SeqCst) >= MAX_PENDING_HANDSHAKES {
                continue; // 直接丢弃（关闭）
            }
            self.pending.fetch_add(1, Ordering::SeqCst);
            let me = self.clone();
            spawn_named("net-conn", move || {
                let est = me.establish(stream, false);
                me.pending.fetch_sub(1, Ordering::SeqCst);
                if let Some((sess, s, opener, first)) = est {
                    me.run_reader(sess, s, opener, first);
                }
            });
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
            let Some(target) = cfg.peer_addr() else { continue };
            if self.psk(&cfg).is_none() {
                continue;
            }
            let mut addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
                Ok(a) => a.collect(),
                Err(e) => {
                    self.note_err(format!("对方地址 {target} 解析失败：{e}"));
                    continue;
                }
            };
            addrs.sort_by_key(|a| !a.is_ipv4()); // IPv4 优先
            for addr in addrs {
                match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
                    Ok(s) => {
                        if let Some((sess, s, opener, first)) = self.establish(s, true) {
                            let me = self.clone();
                            spawn_named("net-conn", move || me.run_reader(sess, s, opener, first));
                        }
                        break;
                    }
                    Err(e) => self.note_err(format!("连接 {addr} 失败：{e}")),
                }
            }
        }
    }

    fn run_udp(self: Arc<Self>) {
        let Some(sock) = self.udp.as_ref() else { return };
        let mut buf = [0u8; 2048];
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                // Windows：之前发往关闭端口的包会让下一次 recv 报 10054，忽略
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            let Some(sess) = self.current() else { continue };
            let Some(plain) = lock(&sess.udp_rx).open(&buf[..n]) else { continue };
            {
                let mut u = lock(&sess.udp_peer);
                u.addr = Some(src);
                u.learned = true;
            }
            if let Some(UdpMsg::Move { epoch, seq, x, y }) = UdpMsg::decode(&plain) {
                self.injector.moved(sess.id, epoch, seq, x, y);
            }
        }
    }

    fn send_udp(&self, s: &Session, m: &UdpMsg) {
        if let Some(sock) = &self.udp {
            s.send_udp(sock, m);
        }
    }

    /// 常驻 writer：独占 net 通道，整个进程生命周期。
    fn run_writer(self: Arc<Self>, rx: Receiver<Out>) {
        let mut q = OutQueue::default();
        let mut img_id = u32::from_le_bytes([crypto::random32()[0], 0, 0, 0]);
        let mut next_tick = Instant::now() + PING_EVERY;
        loop {
            // 有图片在发：不等待，发完一片就回来看看有没有更急的消息
            let first = if q.img.is_some() {
                match rx.try_recv() {
                    Ok(o) => Some(o),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                    Ok(o) => Some(o),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(o) = first {
                q.push(o);
            }
            while let Ok(o) = rx.try_recv() {
                q.push(o);
            }

            let sess = self.current();
            match &sess {
                None => q.clear(),
                Some(s) => {
                    let (frames, mv) = q.take_batch(s.id, &mut img_id);
                    if let Some(mv) = mv {
                        self.send_udp(s, &mv);
                    }
                    for f in frames {
                        if let Err(e) = s.send_plain(&f) {
                            if e.kind() == io::ErrorKind::InvalidInput {
                                crate::log!("丢弃过大的消息（{} 字节）", f.len());
                                continue;
                            }
                            crate::log!("发送失败：{e}");
                            q.clear();
                            break;
                        }
                    }
                }
            }

            if Instant::now() >= next_tick {
                next_tick = Instant::now() + PING_EVERY;
                if let Some(s) = &sess {
                    let _ = s.send_msg(&Msg::Ping { t: config::now_ms() });
                    self.send_udp(s, &UdpMsg::Hi);
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
    data: Vec<u8>,
    header_sent: bool,
    off: usize,
}

#[derive(Default)]
struct OutQueue {
    msgs: VecDeque<Msg>,
    mv: Option<UdpMsg>,
    img: Option<ImgTx>,
}

impl OutQueue {
    fn push(&mut self, o: Out) {
        match o {
            Out::Msg(m) => self.msgs.push_back(m),
            Out::Move { epoch, seq, x, y } => self.mv = Some(UdpMsg::Move { epoch, seq, x, y }),
            Out::ClipImage { format, raw_len, deflated } => {
                if deflated.is_empty() || u32::try_from(deflated.len()).is_err() {
                    return;
                }
                // 新图片顶掉还没发完的旧图片（接收方见到新 id 会丢弃旧的）
                self.img = Some(ImgTx { sess: None, id: 0, format, raw_len, data: deflated, header_sent: false, off: 0 });
            }
        }
    }

    fn clear(&mut self) {
        self.msgs.clear();
        self.mv = None;
        self.img = None;
    }

    /// 取出这一轮要发的：TCP 明文帧（全部消息，然后至多一个图片帧）、最新的一个 Move。
    fn take_batch(&mut self, sess_id: u64, img_id: &mut u32) -> (Vec<Vec<u8>>, Option<UdpMsg>) {
        let mut frames: Vec<Vec<u8>> = self.msgs.drain(..).map(|m| proto::encode_msg(&m)).collect();
        let mv = self.mv.take();
        if self.img.as_ref().is_some_and(|i| i.sess.is_some_and(|s| s != sess_id)) {
            self.img = None;
        }
        if let Some(img) = self.img.as_mut() {
            img.sess = Some(sess_id);
            if !img.header_sent {
                *img_id = img_id.wrapping_add(1);
                img.id = *img_id;
                img.header_sent = true;
                let total = img.data.len() as u32;
                frames.push(proto::encode_msg(&Msg::ClipImage { id: img.id, format: img.format, total, raw_len: img.raw_len }));
            } else {
                let end = (img.off + CHUNK).min(img.data.len());
                frames.push(proto::encode_chunk(img.id, img.off as u32, &img.data[img.off..end]));
                img.off = end;
                if end >= img.data.len() {
                    self.img = None;
                }
            }
        }
        (frames, mv)
    }
}

// ---------------------------------------------------------------- 剪贴板图片拼装（纯逻辑）

struct Incoming {
    id: u32,
    format: u32,
    total: usize,
    raw_len: usize,
    buf: Vec<u8>,
}

#[derive(Default)]
struct ClipRx {
    cur: Option<Incoming>,
}

impl ClipRx {
    fn reset(&mut self) {
        self.cur = None;
    }

    /// 新传输开始（丢弃未完成的旧传输）。超过上限返回 false。
    fn start(&mut self, id: u32, format: u32, total: u32, raw_len: u32, max: usize) -> bool {
        self.cur = None;
        let (total, raw_len) = (total as usize, raw_len as usize);
        if total == 0 || raw_len == 0 || total > max || raw_len > max {
            return false;
        }
        self.cur = Some(Incoming { id, format, total, raw_len, buf: Vec::with_capacity(total.min(4 * CHUNK)) });
        true
    }

    /// 收到一片；拼齐并解压成功时返回 (format, 原始数据)。
    fn chunk(&mut self, id: u32, offset: u32, data: &[u8]) -> Option<(u32, Vec<u8>)> {
        let inc = self.cur.as_mut()?;
        if inc.id != id {
            return None; // 旧传输的残片
        }
        if offset as usize != inc.buf.len() || inc.buf.len() + data.len() > inc.total {
            crate::log!("剪贴板图片分片错位，丢弃");
            self.cur = None;
            return None;
        }
        inc.buf.extend_from_slice(data);
        if inc.buf.len() < inc.total {
            return None;
        }
        let inc = self.cur.take()?;
        match miniz_oxide::inflate::decompress_to_vec_with_limit(&inc.buf, inc.raw_len) {
            Ok(v) if v.len() == inc.raw_len => Some((inc.format, v)),
            Ok(v) => {
                crate::log!("剪贴板图片长度不符（{} ≠ {}），丢弃", v.len(), inc.raw_len);
                None
            }
            Err(e) => {
                crate::log!("剪贴板图片解压失败：{e:?}");
                None
            }
        }
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Side;
    use crate::hub::{HubRx, ToDesktop, ToInput};

    fn psk() -> [u8; 32] {
        [0x42; 32]
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
            let ch = handshake(&mut s, &ks, false).unwrap();
            (s, ch)
        });
        let cc = handshake(&mut c, &kc, true).unwrap();
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
        match proto::decode_frame(&read_frame(&mut s, &mut sc.rx).unwrap()) {
            Some(Frame::Msg(d)) => assert_eq!(d, m),
            _ => panic!("expected msg"),
        }
        match proto::decode_frame(&read_frame(&mut s, &mut sc.rx).unwrap()) {
            Some(Frame::Chunk { id, offset, data }) => assert_eq!((id, offset, data.len()), (9, 0, CHUNK)),
            _ => panic!("expected chunk"),
        }
        // s → c
        write_frame(&mut s, &mut sc.tx, &proto::encode_msg(&Msg::Yield)).unwrap();
        assert!(matches!(proto::decode_frame(&read_frame(&mut c, &mut cc.rx).unwrap()), Some(Frame::Msg(Msg::Yield))));
        // 篡改：直接写一个用错误计数器加密的帧（重放第 0 帧的密文）
        let mut fresh = Sealer::new(&crypto::derive(&psk(), &cc.nonce_c, &[0; 32]).c2s);
        write_frame(&mut c, &mut fresh, b"\0{}").unwrap();
        assert_eq!(read_frame(&mut s, &mut sc.rx).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn wrong_key_fails_first_frame() {
        let (mut c, mut cc, mut s, mut sc) = hs_pair(psk(), [0x43; 32]);
        write_frame(&mut c, &mut cc.tx, &proto::encode_msg(&Msg::Yield)).unwrap();
        assert_eq!(read_frame(&mut s, &mut sc.rx).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_and_bad_magic_rejected() {
        let (mut c, mut s) = tcp_pair();
        c.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes()).unwrap();
        let mut o = Opener::new(&psk());
        assert_eq!(read_frame(&mut s, &mut o).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut sealer = Sealer::new(&psk());
        assert_eq!(write_frame(&mut c, &mut sealer, &vec![0u8; MAX_PLAIN + 1]).unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let (mut c, mut s) = tcp_pair();
        c.write_all(b"XXXX").unwrap();
        c.write_all(&[0u8; 32]).unwrap();
        assert!(handshake(&mut s, &psk(), false).is_err());
    }

    #[test]
    fn udp_seal_open_over_sockets() {
        let (_, cc, _, sc) = hs_pair(psk(), psk());
        let (mut utx, mut urx) = (cc.utx, sc.urx);
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mv = UdpMsg::Move { epoch: 1, seq: 5, x: 100, y: -20 };
        let p1 = utx.seal(&mv.encode());
        let p2 = utx.seal(&UdpMsg::Hi.encode());
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
                Some(Frame::Msg(Msg::ClipImage { .. })) => "hdr".into(),
                Some(Frame::Msg(Msg::Ping { t })) => format!("p{t}"),
                Some(Frame::Msg(_)) => "msg".into(),
                Some(Frame::Chunk { offset, .. }) => format!("c{offset}"),
                None => "?".into(),
            })
            .collect()
    }

    #[test]
    fn queue_priority_and_coalescing() {
        let mut q = OutQueue::default();
        let mut id = 0u32;
        let data: Vec<u8> = (0..CHUNK * 2 + 10).map(|i| i as u8).collect();
        q.push(Out::ClipImage { format: 8, raw_len: 999, deflated: data.clone() });
        q.push(mv(1));
        q.push(Out::Msg(Msg::Ping { t: 1 }));
        q.push(mv(2));
        q.push(Out::Msg(Msg::Ping { t: 2 }));
        let (f, m) = q.take_batch(7, &mut id);
        assert_eq!(kinds(&f), ["p1", "p2", "hdr"]);
        assert_eq!(m, Some(UdpMsg::Move { epoch: 1, seq: 2, x: 2, y: 0 }));
        let (f, m) = q.take_batch(7, &mut id);
        assert_eq!(kinds(&f), ["c0"]);
        assert_eq!(m, None);
        // 分片之间插入的消息先发
        q.push(Out::Msg(Msg::Ping { t: 3 }));
        let (f, _) = q.take_batch(7, &mut id);
        assert_eq!(kinds(&f), ["p3", &format!("c{CHUNK}")]);
        let (f, _) = q.take_batch(7, &mut id);
        assert_eq!(kinds(&f), [format!("c{}", CHUNK * 2)]);
        assert!(q.img.is_none());
        let (f, _) = q.take_batch(7, &mut id);
        assert!(f.is_empty());
    }

    #[test]
    fn queue_drops_image_on_session_change() {
        let mut q = OutQueue::default();
        let mut id = 0u32;
        q.push(Out::ClipImage { format: 8, raw_len: 1, deflated: vec![1; CHUNK * 3] });
        let (f, _) = q.take_batch(1, &mut id);
        assert_eq!(kinds(&f), ["hdr"]);
        let (f, _) = q.take_batch(2, &mut id);
        assert!(f.is_empty());
        assert!(q.img.is_none());
        // 新图片替换旧图片，id 递增
        q.push(Out::ClipImage { format: 8, raw_len: 1, deflated: vec![1; 10] });
        q.push(Out::ClipImage { format: 17, raw_len: 1, deflated: vec![2; 10] });
        let (f, _) = q.take_batch(2, &mut id);
        match proto::decode_frame(&f[0]) {
            Some(Frame::Msg(Msg::ClipImage { id, format, total, .. })) => assert_eq!((id, format, total), (2, 17, 10)),
            _ => panic!(),
        }
    }

    fn deflate(raw: &[u8]) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec(raw, 1)
    }

    #[test]
    fn clip_reassembly() {
        let raw: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8 ^ (i / 1000) as u8).collect();
        let z = deflate(&raw);
        let mut rx = ClipRx::default();
        assert!(rx.start(5, 8, z.len() as u32, raw.len() as u32, 1 << 20));
        let mut out = None;
        for (i, part) in z.chunks(1000).enumerate() {
            assert!(out.is_none());
            out = rx.chunk(5, (i * 1000) as u32, part);
        }
        assert_eq!(out, Some((8, raw.clone())));

        // 超上限
        assert!(!rx.start(6, 8, z.len() as u32, raw.len() as u32, 1000));
        assert!(rx.chunk(6, 0, &z).is_none());
        // 新 id 丢弃旧传输
        assert!(rx.start(7, 8, z.len() as u32, raw.len() as u32, 1 << 20));
        assert!(rx.chunk(7, 0, &z[..10]).is_none());
        assert!(rx.start(8, 17, z.len() as u32, raw.len() as u32, 1 << 20));
        assert!(rx.chunk(7, 10, &z[10..]).is_none());
        assert_eq!(rx.chunk(8, 0, &z), Some((17, raw.clone())));
        // 偏移错位
        assert!(rx.start(9, 8, z.len() as u32, raw.len() as u32, 1 << 20));
        assert!(rx.chunk(9, 5, &z[..10]).is_none());
        assert!(rx.cur.is_none());
        // raw_len 声称比实际小：解压超限 → 丢弃
        assert!(rx.start(10, 8, z.len() as u32, 100, 1 << 20));
        assert!(rx.chunk(10, 0, &z).is_none());
        // 垃圾数据
        assert!(rx.start(11, 8, 4, 100, 1 << 20));
        assert!(rx.chunk(11, 0, &[0xff; 4]).is_none());
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
        let mut cfg = Config { name: name.into(), port, peer, key: key.into(), ..Default::default() };
        cfg.layout.layout = layout;
        cfg.layout.rev = rev;
        let (hub, HubRx { input, desktop, net }) = Hub::new(cfg);
        let hub = Arc::new(hub);
        let injector = Arc::new(Injector::new(hub.clone()));
        let opts = Opts { bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST), persist: false, dial_every: Duration::from_millis(100) };
        let net = start(hub.clone(), net, injector, opts);
        Node { net, hub, input, desktop }
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

    fn next_desktop(rx: &Receiver<ToDesktop>, secs: u64) -> ToDesktop {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(ToDesktop::Status) => continue,
                Ok(m) => return m,
                Err(_) => panic!("desktop 消息超时"),
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

        // 图片：多片，中间夹着文字
        let mut raw = vec![0u8; 300_000];
        getrandom::fill(&mut raw).unwrap();
        raw[..100_000].fill(7); // 一部分可压缩
        let z = deflate(&raw);
        assert!(z.len() > CHUNK);
        b.hub.to_net(Out::ClipImage { format: 17, raw_len: raw.len() as u32, deflated: z });
        b.hub.send(Msg::ClipText { text: "插队".into() });
        let mut got_text = false;
        let mut got_img = false;
        for _ in 0..2 {
            match next_desktop(&a.desktop, 10) {
                ToDesktop::ClipText(t) => {
                    assert_eq!(t, "插队");
                    assert!(!got_img, "文字应先于图片完成");
                    got_text = true;
                }
                ToDesktop::ClipImage { format, data } => {
                    assert_eq!(format, 17);
                    assert!(data == raw);
                    got_img = true;
                }
                m => panic!("{m:?}"),
            }
        }
        assert!(got_text && got_img);

        // UDP：双方通过 Hi 学到对方地址
        let learned = |n: &Node| n.net.current().is_some_and(|s| lock(&s.udp_peer).learned);
        wait("UDP 打通", 5, || learned(&a) && learned(&b));
        let sa = a.net.current().unwrap();
        assert_eq!(lock(&sa.udp_peer).addr.unwrap().port(), pb);

        // Ping/Pong 保持连接，会话不变
        let id_before = sa.id;
        std::thread::sleep(Duration::from_millis(1500));
        assert!(a.hub.status().connected && b.hub.status().connected);
        assert_eq!(a.net.current().unwrap().id, id_before);
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
}
