//! 声音同步（两台的声音混在一起放）：控制线程 + 采集发送线程 + 接收播放线程。
//!
//! 模式（config [audio] mode，本机视角，两台同步）：关 / 本机听（here）/ 对方听（there）/ 两台放（both）。
//!   send = there|both：采本机除 kmlink 外的所有声音（进程环回，排除自己 → 收到的声音不会再被采回去，没有回声环路），
//!          每 5ms 一包 PCM 走 UDP（和鼠标同一个端口，独立密钥/计数器，见 crypto::AudioSealer）。静音超过 200ms 不发。
//!   play = here|both：收到的包进抗抖动缓冲（jitter.rs），按声卡节奏放出来。
//!   there：对方回报“正在放”（AudioAck，每秒一次）才把本机默认输出设备静音；3 秒收不到、模式变了、断开、退出都恢复。
//!          只恢复自己静音的（用户本来就静音的不碰）；崩溃后下次启动按标记文件恢复。
//!   “暂停共享”不影响声音。
//!
//! 线程：
//!   - audio-ctl：一直在（每 200ms 或有事时醒），管模式同步、开关下面两个线程、Ack、静音。COM 只在这里和下面两个线程里用。
//!   - audio-cap：send 时才有。直接调 net 的发送函数（加密 + send_to），不经队列。
//!   - audio-play：play 时才有。net-udp（最高优先级）解密后只往无锁环里放一包、置事件，不做别的；play 线程取出来进缓冲。

use crate::audio_cap::{self, Capture};
use crate::audio_play::{self, Render};
use crate::config::{self, AudioMode};
use crate::hub::Hub;
use crate::jitter::{self, FLAG_START, Jitter, MAX_PLAIN, SAMPLES};
use crate::proto::{self, CAP_AUDIO, Msg};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

/// 控制线程多久看一次。
const TICK: Duration = Duration::from_millis(200);
/// 播放端多久回一次 Ack；发送端多久收不到就恢复本机声音。
const ACK_EVERY: Duration = Duration::from_secs(1);
const ACK_STALE: Duration = Duration::from_secs(3);
/// 静音门限（峰值，约 -80 dBFS）和挂起时间（包数：200ms）。
const GATE: f32 = 1e-4;
const GATE_HANG: u32 = 40;
/// 这么久没收到包且缓冲放空：关掉输出流（不占声卡）。
const PLAY_IDLE: Duration = Duration::from_secs(2);
/// 打开采集/输出失败后多久重试。
const RETRY: Duration = Duration::from_secs(2);
/// 收包环：64 包 = 320ms（play 线程每个包都会被唤醒取走，正常只会攒一两包）。
const RING: usize = 64;
/// 对方的 rev 至多比本机时间超前这么多（毫秒）。
const MAX_REV_AHEAD: u64 = 24 * 3600 * 1000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    crate::smooth::now_us() as u64 / 1000
}

// ---------- net-udp → audio-play 的无锁单生产者单消费者环 ----------

struct Slot {
    len: usize,
    arrive_us: u32,
    data: [u8; MAX_PLAIN],
}

struct Ring {
    slots: Box<[UnsafeCell<Slot>]>,
    /// 写过的总数 / 读过的总数。
    head: AtomicUsize,
    tail: AtomicUsize,
}

// 只有 net-udp 写、只有当前 play 线程读（换 play 线程前旧的已 join）
unsafe impl Sync for Ring {}

impl Ring {
    fn new() -> Ring {
        let slots = (0..RING).map(|_| UnsafeCell::new(Slot { len: 0, arrive_us: 0, data: [0; MAX_PLAIN] })).collect();
        Ring { slots, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }
    /// 满了返回 false（丢这一包）。
    fn push(&self, data: &[u8], arrive_us: u32) -> bool {
        if data.len() > MAX_PLAIN {
            return false;
        }
        let h = self.head.load(Ordering::Relaxed);
        if h.wrapping_sub(self.tail.load(Ordering::Acquire)) >= RING {
            return false;
        }
        // SAFETY：单生产者；这个槽读者已经读完（tail 之后）
        let s = unsafe { &mut *self.slots[h % RING].get() };
        s.len = data.len();
        s.arrive_us = arrive_us;
        s.data[..data.len()].copy_from_slice(data);
        self.head.store(h.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self, f: impl FnOnce(&[u8], u32)) -> bool {
        let t = self.tail.load(Ordering::Relaxed);
        if t == self.head.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY：单消费者；head 之前的槽写者已经写完
        let s = unsafe { &*self.slots[t % RING].get() };
        f(&s.data[..s.len], s.arrive_us);
        self.tail.store(t.wrapping_add(1), Ordering::Release);
        true
    }
    fn clear(&self) {
        while self.pop(|_, _| {}) {}
    }
}

// ---------- 对外接口 ----------

enum Note {
    Up { sid: u64, peer_id: u64, name: String },
    Down { sid: u64 },
    PeerMode { sid: u64, peer_id: u64, mode: String, rev: u64 },
    PeerAck { playing: bool },
    Quit(std::sync::mpsc::Sender<()>),
}

type SendFn = Box<dyn Fn(u64, &[u8]) -> bool + Send + Sync>;

/// net 和声音线程之间的接口。正式运行一个进程一个（main 里建）。
pub struct Port {
    hub: Arc<Hub>,
    /// 当前会话号（对方支持声音才有；0 = 没有）。net-udp 据此决定收不收声音包。
    sid: AtomicU64,
    /// 正在放（play 线程开着），net-udp 才往环里放。
    accept: AtomicBool,
    ring: OnceLock<Ring>,
    /// 有包到了：唤醒 play 线程（自动复位事件）。
    wake: OnceLock<isize>,
    notes: Mutex<VecDeque<Note>>,
    cv: Condvar,
    sender: OnceLock<SendFn>,
    /// play 线程能放（这次收到过对方的包，且输出流开着或空闲关着但没打开失败）：控制线程据此回 Ack。
    playing: AtomicBool,
    /// 本机采集在发、最近一次发送成功了：“对方听”只有这样才静音本机（UDP 不通时不能把两边都弄没声）。
    send_ok: AtomicBool,
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
}

impl Port {
    pub fn new(hub: Arc<Hub>) -> Arc<Port> {
        Arc::new(Port {
            hub,
            sid: AtomicU64::new(0),
            accept: AtomicBool::new(false),
            ring: OnceLock::new(),
            wake: OnceLock::new(),
            notes: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            sender: OnceLock::new(),
            playing: AtomicBool::new(false),
            send_ok: AtomicBool::new(false),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
        })
    }

    /// net 启动时登记发送函数：(会话号, 明文) → 是否发出去了（加密、send_to 都在 net 里）。
    pub fn set_sender(&self, f: SendFn) {
        let _ = self.sender.set(f);
    }

    fn note(&self, n: Note) {
        lock(&self.notes).push_back(n);
        self.cv.notify_one();
    }

    /// 收到对方 Hello（reader 线程）。
    pub fn session_up(&self, sid: u64, caps: u32, peer_id: u64, name: &str) {
        if caps & CAP_AUDIO == 0 {
            let old = self.sid.swap(0, Ordering::AcqRel);
            if old != 0 {
                self.note(Note::Down { sid: old });
            }
            return;
        }
        self.sid.store(sid, Ordering::Release);
        self.note(Note::Up { sid, peer_id, name: name.to_string() });
    }

    /// 会话结束（任意 net 线程）。
    pub fn session_down(&self, sid: u64) {
        if self.sid.compare_exchange(sid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.note(Note::Down { sid });
        }
    }

    pub fn peer_mode(&self, sid: u64, peer_id: u64, mode: String, rev: u64) {
        self.note(Note::PeerMode { sid, peer_id, mode, rev });
    }

    pub fn peer_ack(&self, playing: bool) {
        self.note(Note::PeerAck { playing });
    }

    /// net-udp 线程：解密后的声音包。只放进环、置事件，不阻塞。
    pub fn packet(&self, sid: u64, plain: &[u8], arrive_us: u32) {
        if sid != self.sid.load(Ordering::Acquire) || !self.accept.load(Ordering::Acquire) {
            return;
        }
        let (Some(r), Some(&w)) = (self.ring.get(), self.wake.get()) else { return };
        if r.push(plain, arrive_us) {
            self.rx_packets.fetch_add(1, Ordering::Relaxed);
            unsafe {
                let _ = SetEvent(HANDLE(w as *mut _));
            }
        }
    }

    fn send(&self, sid: u64, plain: &[u8]) -> bool {
        let ok = self.sender.get().is_some_and(|f| f(sid, plain));
        if ok {
            self.tx_packets.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// 退出前：停线程、恢复本机静音（最多等 1 秒）。
    pub fn shutdown(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.note(Note::Quit(tx));
        let _ = rx.recv_timeout(Duration::from_secs(1));
    }
}

/// 起控制线程。
pub fn spawn(port: Arc<Port>) {
    let r = std::thread::Builder::new().name("audio-ctl".into()).stack_size(256 * 1024).spawn(move || Ctl::new(port).run());
    if let Err(e) = r {
        crate::log!("声音：控制线程启动失败：{e}");
    }
}

// ---------- 控制线程 ----------

struct Worker {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl Worker {
    fn start(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Option<Worker> {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        match std::thread::Builder::new().name(name.into()).stack_size(256 * 1024).spawn(move || f(s2)) {
            Ok(join) => Some(Worker { stop, join }),
            Err(e) => {
                crate::log!("声音：{name} 线程启动失败：{e}");
                None
            }
        }
    }
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.join.join();
    }
}

struct Ctl {
    port: Arc<Port>,
    /// 当前会话 (sid, 对方 instance id, 对方名字)。
    sess: Option<(u64, u64, String)>,
    /// 本会话已发给对方的 (sid, mode, rev)。
    sent: Option<(u64, AudioMode, u64)>,
    cap: Option<(u64, Worker)>,
    play: Option<Worker>,
    ack_at: Option<Instant>,
    /// 对方最近一次说“正在放”。
    peer_playing: Option<Instant>,
    mute: Muter,
    my_id: u64,
}

impl Ctl {
    fn new(port: Arc<Port>) -> Ctl {
        Ctl {
            port,
            sess: None,
            sent: None,
            cap: None,
            play: None,
            ack_at: None,
            peer_playing: None,
            mute: Muter::default(),
            my_id: proto::instance_id(),
        }
    }

    fn run(mut self) {
        audio_cap::com_init();
        self.mute.recover();
        self.claim_hand_edit();
        loop {
            let notes: Vec<Note> = {
                let g = lock(&self.port.notes);
                let (mut g, _) = self.port.cv.wait_timeout_while(g, TICK, |q| q.is_empty()).unwrap_or_else(|e| e.into_inner());
                g.drain(..).collect()
            };
            for n in notes {
                match n {
                    Note::Up { sid, peer_id, name } => {
                        // 同一会话重发的 Hello（显示器/名字变了）：不清对方的播放状态（不然“对方听”会短暂放开静音）
                        if self.sess.as_ref().is_none_or(|s| s.0 != sid) {
                            self.peer_playing = None;
                        }
                        self.sess = Some((sid, peer_id, name));
                    }
                    Note::Down { sid } => {
                        if self.sess.as_ref().is_some_and(|s| s.0 == sid) {
                            self.sess = None;
                            self.peer_playing = None;
                        }
                    }
                    Note::PeerMode { sid, peer_id, mode, rev } => self.peer_mode(sid, peer_id, &mode, rev),
                    Note::PeerAck { playing } => self.peer_playing = playing.then(Instant::now),
                    Note::Quit(done) => {
                        // 先恢复静音再停线程：线程可能卡在打开设备里（最多 5 秒），进程马上就退出了
                        self.mute.retry_at = None;
                        self.mute.unmute();
                        let _ = done.send(());
                        self.sess = None;
                        self.apply(AudioMode::Off);
                        return;
                    }
                }
            }
            let mode = self.port.hub.cfg().audio.mode;
            self.sync_mode();
            self.apply(mode);
        }
    }

    /// 启动时：kmlink 没在跑时手改了模式 → rev 变新（不然连上后可能被对方的旧模式盖掉）。
    fn claim_hand_edit(&mut self) {
        let new_cfg = {
            let mut c = self.port.hub.cfg.write().unwrap_or_else(|e| e.into_inner());
            if !c.audio.claim_hand_edit() {
                return;
            }
            c.clone()
        };
        let a = new_cfg.audio.clone();
        let r = config::update(&new_cfg, |d| {
            if d.audio.mode == a.mode {
                d.audio.rev = d.audio.rev.max(a.rev);
                d.audio.rev_mode = a.rev_mode;
            }
        });
        if let Err(e) = r {
            crate::log!("保存配置失败：{e}");
        }
    }

    /// 把本机模式发给对方（变了才发；新会话重发）。
    fn sync_mode(&mut self) {
        let Some((sid, ..)) = self.sess else { return };
        let a = self.port.hub.cfg().audio;
        if self.sent == Some((sid, a.mode, a.rev)) {
            return;
        }
        self.sent = Some((sid, a.mode, a.rev));
        self.port.hub.send(Msg::Audio { mode: a.mode.as_str().into(), rev: a.rev });
    }

    fn peer_mode(&mut self, sid: u64, peer_id: u64, mode: &str, rev: u64) {
        if rev > config::now_ms().saturating_add(MAX_REV_AHEAD) {
            crate::log!("声音：忽略对方不合理的模式版本 {rev}");
            return;
        }
        let theirs = AudioMode::parse(mode).mirrored();
        let new_cfg = {
            let mut c = self.port.hub.cfg.write().unwrap_or_else(|e| e.into_inner());
            if !adopt(c.audio.rev, c.audio.mode, theirs, rev, self.my_id, peer_id) {
                if c.audio.mode != theirs {
                    // 本机的赢：再发一次，让对方采用（不然两台会停在不同模式）
                    self.sent = None;
                }
                return;
            }
            c.audio.mode = theirs;
            c.audio.rev = rev;
            c.audio.rev_mode = Some(theirs);
            c.clone()
        };
        // 刚采用的不用回发
        self.sent = Some((sid, theirs, rev));
        crate::log!("声音：采用对方的模式 → 本机 {}", theirs.as_str());
        let r = config::update(&new_cfg, |c| {
            if c.audio.rev <= rev {
                c.audio.mode = theirs;
                c.audio.rev = rev;
                c.audio.rev_mode = Some(theirs);
            }
        });
        if let Err(e) = r {
            crate::log!("保存配置失败：{e}");
        }
    }

    /// 按模式和连接状态开关采集/播放线程、回 Ack、静音。
    fn apply(&mut self, mode: AudioMode) {
        let sid = self.sess.as_ref().map(|s| s.0);
        // 采集：会话变了要重起（发送函数按会话号发）
        let want_cap = sid.filter(|_| mode.send());
        if self.cap.as_ref().map(|c| c.0) != want_cap {
            if let Some((_, w)) = self.cap.take() {
                w.stop();
                self.port.send_ok.store(false, Ordering::Release);
                crate::log!("声音：停止发送（共发 {} 包）", self.port.tx_packets.load(Ordering::Relaxed));
            }
            if let Some(sid) = want_cap {
                let port = self.port.clone();
                if let Some(w) = Worker::start("audio-cap", move |stop| capture_main(port, stop, sid)) {
                    crate::log!("声音：开始发送本机声音");
                    self.cap = Some((sid, w));
                }
            }
        }
        // 播放：会话断开也停（输出流本来空闲 2 秒就关了，这里连线程一起停）
        let want_play = sid.is_some() && mode.play();
        if self.play.is_some() != want_play {
            if let Some(w) = self.play.take() {
                self.port.accept.store(false, Ordering::Release);
                w.stop();
                self.port.playing.store(false, Ordering::Release);
                crate::log!("声音：停止播放对方声音（共收 {} 包）", self.port.rx_packets.load(Ordering::Relaxed));
            } else if self.init_ring() {
                // 先清环再起 play 线程：任何时候只有一个读者（旧 play 线程已 join，accept 是 false）
                if let Some(r) = self.port.ring.get() {
                    r.clear();
                }
                let port = self.port.clone();
                let name = format!("{} 的声音", self.sess.as_ref().map(|s| s.2.as_str()).unwrap_or("对方"));
                if let Some(w) = Worker::start("audio-play", move |stop| play_main(port, stop, name)) {
                    self.port.accept.store(true, Ordering::Release);
                    crate::log!("声音：准备播放对方声音");
                    self.play = Some(w);
                }
            }
        }
        // Ack：本机能放对方的声音（输出流空闲关掉了也算，打开失败才不算）
        if want_play && self.ack_at.is_none_or(|t| t.elapsed() >= ACK_EVERY) {
            self.ack_at = Some(Instant::now());
            self.port.hub.send(Msg::AudioAck { playing: self.port.playing.load(Ordering::Acquire) });
        }
        // 对方听：对方确认在放才静音本机
        let peer_ok = self.peer_playing.is_some_and(|t| t.elapsed() < ACK_STALE);
        let sending = self.cap.is_some() && self.port.send_ok.load(Ordering::Acquire);
        if mode == AudioMode::There && want_cap.is_some() && peer_ok && sending {
            self.mute.mute();
        } else {
            self.mute.unmute();
        }
    }

    fn init_ring(&self) -> bool {
        self.port.ring.get_or_init(Ring::new);
        if self.port.wake.get().is_none() {
            match unsafe { CreateEventW(None, false, false, None) } {
                Ok(h) => {
                    let _ = self.port.wake.set(h.0 as isize);
                }
                Err(e) => {
                    crate::log!("声音：建事件失败：{e}");
                    return false;
                }
            }
        }
        true
    }
}

/// 采用对方的模式吗（同排布：rev 大的赢；rev 相同模式不同，instance id 小的让）。
fn adopt(my_rev: u64, mine: AudioMode, theirs: AudioMode, rev: u64, my_id: u64, peer_id: u64) -> bool {
    if rev != my_rev {
        return rev > my_rev;
    }
    theirs != mine && my_id < peer_id
}

// ---------- 采集发送线程 ----------

fn capture_main(port: Arc<Port>, stop: Arc<AtomicBool>, sid: u64) {
    audio_cap::com_init();
    crate::sys::boost_thread(false);
    audio_play::mmcss();
    let mut seq: u32 = 0;
    let mut acc: Vec<f32> = Vec::with_capacity(SAMPLES * 8);
    let mut quiet = GATE_HANG; // 一开始算静音：有声音了才发，第一包带 START
    let mut warned = false;
    let (mut sent, mut failed, mut log_at) = (0u64, 0u64, Instant::now());
    port.send_ok.store(false, Ordering::Release);
    while !stop.load(Ordering::Acquire) {
        let mut cap = match Capture::open(false) {
            Ok(c) => c,
            Err(e) => {
                port.send_ok.store(false, Ordering::Release);
                if !warned {
                    warned = true;
                    crate::log!("声音：打开采集失败（{e}），稍后重试");
                }
                sleep_unless(&stop, RETRY);
                continue;
            }
        };
        warned = false;
        acc.clear();
        while !stop.load(Ordering::Acquire) {
            if let Err(e) = cap.read(50, |s, _| acc.extend_from_slice(s)) {
                crate::log!("声音：采集出错（{e}），重新打开");
                break;
            }
            let mut off = 0;
            while acc.len() - off >= SAMPLES {
                let f = &acc[off..off + SAMPLES];
                off += SAMPLES;
                let peak = f.iter().fold(0f32, |m, v| m.max(v.abs()));
                let mut flags = 0;
                if peak > GATE {
                    if quiet >= GATE_HANG {
                        flags = FLAG_START;
                    }
                    quiet = 0;
                } else {
                    quiet = quiet.saturating_add(1);
                }
                if quiet < GATE_HANG {
                    let p = jitter::encode_pcm(seq, flags, crate::smooth::stamp(), f);
                    let ok = port.send(sid, &p);
                    port.send_ok.store(ok, Ordering::Release);
                    if ok {
                        sent += 1;
                    } else {
                        failed += 1;
                    }
                    seq = seq.wrapping_add(1);
                }
            }
            acc.drain(..off);
            if log_at.elapsed() >= Duration::from_secs(60) {
                log_at = Instant::now();
                if sent + failed > 0 {
                    crate::log!("声音：发 {sent} 包，发送失败 {failed}");
                }
                (sent, failed) = (0, 0);
            }
        }
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d && !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------- 接收播放线程 ----------

fn play_main(port: Arc<Port>, stop: Arc<AtomicBool>, name: String) {
    audio_cap::com_init();
    crate::sys::boost_thread(false);
    audio_play::mmcss();
    let (Some(ring), Some(&wake)) = (port.ring.get(), port.wake.get()) else { return };
    let wake = HANDLE(wake as *mut _);
    let cfg = port.hub.cfg().audio;
    let mut tune = cfg.tune();
    // 上次学到的缓冲深度：从它起步（刚启动时还没见过卡顿，从默认值起步会在头几次卡顿时断音）
    let saved = std::fs::read_to_string(target_path()).ok().and_then(|t| t.trim().parse::<u32>().ok());
    if let Some(ms) = saved {
        tune.start_ms = ms;
    }
    let mut saved_ms = saved.unwrap_or(0);
    let mut vol = cfg.volume.clamp(0.0, 1.0);
    let mut j = Jitter::new(tune);
    let mut out: Option<Render> = None;
    let mut last_pkt: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;
    let mut warned = false;
    let mut cfg_at = Instant::now();
    let mut log_at = Instant::now();
    let mut logged = jitter::Stats::default();
    // 排查用：%APPDATA%\kmlink\audio-trace 存在就把每包的 seq / 发送时刻 / 到达时刻记到 audio-trace.txt（kmlink audio-sim 离线重放）
    let mut trace = std::fs::metadata(config::dir().join("audio-trace")).is_ok().then(|| {
        std::fs::File::create(config::dir().join("audio-trace.txt")).ok().map(std::io::BufWriter::new)
    }).flatten();
    let mut trace_at = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let mut hs = vec![wake];
        if let Some(r) = &out {
            hs.push(r.event);
        }
        unsafe {
            let _ = WaitForMultipleObjects(&hs, false, 20);
        }
        let now = now_ms();
        let mut got = false;
        while ring.pop(|d, arrive| {
            if let Some((h, data)) = jitter::decode(d) {
                if let Some(t) = &mut trace {
                    use std::io::Write;
                    let _ = writeln!(t, "{} {} {} {}", h.seq, h.send_us, arrive, h.flags);
                }
                j.push(h, data, arrive, now);
                got = true;
            }
        }) {}
        if let Some(t) = &mut trace {
            if trace_at.elapsed() >= Duration::from_secs(1) {
                trace_at = Instant::now();
                use std::io::Write;
                let _ = t.flush();
            }
        }
        if got {
            last_pkt = Some(Instant::now());
        }
        // 每秒重读一次音量/缓冲参数
        if cfg_at.elapsed() >= Duration::from_secs(1) {
            cfg_at = Instant::now();
            let c = port.hub.cfg().audio;
            vol = c.volume.clamp(0.0, 1.0);
            let mut t = c.tune();
            t.start_ms = tune.start_ms;
            if t != tune {
                tune = t;
                j.set_tune(tune);
            }
        }
        let recent = last_pkt.is_some_and(|t| t.elapsed() < PLAY_IDLE);
        if out.is_none() && recent && retry_at.is_none_or(|t| Instant::now() >= t) {
            match Render::open(&name) {
                Ok(r) => {
                    if !r.rate_adjust() {
                        crate::log!("声音：输出流没有速率微调");
                    }
                    out = Some(r);
                    warned = false;
                    retry_at = None;
                }
                Err(e) => {
                    if !warned {
                        warned = true;
                        crate::log!("声音：打开输出设备失败（{e}），稍后重试");
                    }
                    retry_at = Some(Instant::now() + RETRY);
                }
            }
        }
        if let Some(r) = &mut out {
            let res = r.write(|o| {
                j.pull(o, now);
                if vol < 1.0 {
                    o.iter_mut().for_each(|v| *v *= vol);
                }
            });
            if let Err(e) = res {
                crate::log!("声音：输出出错（{e}），重新打开");
                out = None;
            } else {
                r.set_ratio(j.ratio());
                if !recent && j.depth() == 0 {
                    out = None; // 空闲：关掉输出流
                }
            }
        }
        // “能放”：开着，或空闲关着但上次打开没失败（“对方听”那台据此保持静音，静音间隙不放开）
        port.playing.store(last_pkt.is_some() && (out.is_some() || retry_at.is_none()), Ordering::Release);
        if log_at.elapsed() >= Duration::from_secs(60) {
            log_at = Instant::now();
            let t = j.target_ms();
            if j.stats.packets > 0 && t != saved_ms {
                saved_ms = t;
                let _ = std::fs::write(target_path(), t.to_string());
            }
            let s = j.stats;
            if s != logged {
                crate::log!(
                    "声音：收 {} 包，迟到 {}，补偿 {}，欠载 {}，丢 {}，缓冲 {}ms",
                    s.packets - logged.packets,
                    s.late - logged.late,
                    s.concealed - logged.concealed,
                    s.underruns - logged.underruns,
                    s.dropped - logged.dropped,
                    j.target_ms()
                );
                logged = s;
            }
        }
    }
    port.playing.store(false, Ordering::Release);
}

/// 学到的缓冲深度（毫秒），下次启动从它起步。
fn target_path() -> std::path::PathBuf {
    config::dir().join("audio-target")
}

// ---------- 本机静音（“对方听”） ----------

#[derive(Default)]
struct Muter {
    /// 当前为“对方听”静音的默认设备 id。
    cur: Option<String>,
    /// 我们静音过、还没成功恢复的设备（含 cur；设备拔掉了恢复不了就留着，插回来再试）。也写在标记文件里，崩溃后下次启动恢复。
    muted: Vec<String>,
    checked: Option<Instant>,
    retry_at: Option<Instant>,
    /// 已记过“拿不到设备 / 用户自己静音了”的设备（同一个只记一次）。
    noted: Option<String>,
}

fn marker() -> std::path::PathBuf {
    config::dir().join("audio-muted")
}

mod ep {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
    use windows::core::PCWSTR;

    fn en() -> Option<IMMDeviceEnumerator> {
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() }
    }
    fn id(d: &IMMDevice) -> Option<String> {
        unsafe {
            let p = d.GetId().ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        }
    }
    pub fn default() -> Option<(String, IAudioEndpointVolume)> {
        unsafe {
            let d = en()?.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
            Some((id(&d)?, d.Activate(CLSCTX_ALL, None).ok()?))
        }
    }
    pub fn by_id(id: &str) -> Option<IAudioEndpointVolume> {
        let w = crate::sys::wide(id);
        unsafe { en()?.GetDevice(PCWSTR(w.as_ptr())).ok()?.Activate(CLSCTX_ALL, None).ok() }
    }
}

impl Muter {
    fn save(&self) {
        if self.muted.is_empty() {
            let _ = std::fs::remove_file(marker());
        } else {
            let _ = std::fs::write(marker(), self.muted.join("\n"));
        }
    }

    /// 上次崩溃留下的静音：记下来，马上恢复（设备不在就之后再试）。
    fn recover(&mut self) {
        if let Ok(t) = std::fs::read_to_string(marker()) {
            self.muted = t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
            if !self.muted.is_empty() {
                crate::log!("声音：恢复上次留下的本机静音");
            }
            self.restore();
        }
    }

    /// 恢复 muted 里除 cur 以外的设备；成功的移出。
    fn restore(&mut self) {
        let before = self.muted.len();
        let cur = self.cur.clone();
        self.muted.retain(|id| {
            if cur.as_deref() == Some(id.as_str()) {
                return true;
            }
            let ok = ep::by_id(id).is_some_and(|v| unsafe { v.SetMute(false, std::ptr::null()) }.is_ok());
            !ok
        });
        if self.muted.len() != before {
            self.save();
        }
        self.retry_at = (self.muted.len() > usize::from(self.cur.is_some())).then(|| Instant::now() + Duration::from_secs(3));
    }

    fn mute(&mut self) {
        // 每秒看一次默认设备换没换（插耳机）
        if self.checked.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.checked = Some(Instant::now());
        let Some((id, v)) = ep::default() else {
            if self.noted.as_deref() != Some("") {
                self.noted = Some(String::new());
                crate::log!("声音：拿不到默认输出设备，没法静音本机");
            }
            return;
        };
        if self.cur.as_deref() == Some(id.as_str()) {
            return;
        }
        // 默认设备换了：恢复旧的
        self.cur = None;
        self.restore();
        // 用户自己已经静音了：不碰，也不记进标记（恢复时不会替用户打开）
        if unsafe { v.GetMute() }.is_ok_and(|b| b.as_bool()) {
            if self.noted.as_deref() != Some(id.as_str()) {
                crate::log!("声音：本机输出设备本来就是静音，不动它");
                self.noted = Some(id);
            }
            return;
        }
        if unsafe { v.SetMute(true, std::ptr::null()) }.is_ok() {
            crate::log!("声音：对方在放本机声音，本机静音");
            if !self.muted.contains(&id) {
                self.muted.push(id.clone());
            }
            self.cur = Some(id);
            self.save();
        }
    }

    /// 恢复我们静音过的所有设备（没有就什么都不做；失败的每 3 秒重试）。
    fn unmute(&mut self) {
        if self.muted.is_empty() {
            self.cur = None;
            return;
        }
        let was = self.cur.take().is_some();
        if !was && self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.restore();
        self.checked = None;
        if was {
            crate::log!("声音：恢复本机声音");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_spsc_order_and_full() {
        let r = Ring::new();
        for i in 0..RING {
            assert!(r.push(&[i as u8; 3], i as u32));
        }
        assert!(!r.push(&[0], 0), "满了丢");
        let mut got = Vec::new();
        while r.pop(|d, a| got.push((d[0], d.len(), a))) {}
        assert_eq!(got.len(), RING);
        assert!(got.iter().enumerate().all(|(i, &(b, n, a))| b == i as u8 && n == 3 && a == i as u32));
        assert!(r.push(&[9; 2], 1));
        assert!(!r.push(&[0; MAX_PLAIN + 1], 0));
    }

    #[test]
    fn ring_threads() {
        let r = Arc::new(Ring::new());
        let w = r.clone();
        let t = std::thread::spawn(move || {
            let mut i = 0u32;
            while i < 20_000 {
                if w.push(&i.to_le_bytes(), i) {
                    i += 1;
                }
            }
        });
        let mut want = 0u32;
        while want < 20_000 {
            r.pop(|d, a| {
                assert_eq!(u32::from_le_bytes(d.try_into().unwrap()), want);
                assert_eq!(a, want);
                want += 1;
            });
        }
        t.join().unwrap();
    }

    #[test]
    fn adopt_rules() {
        use AudioMode::*;
        assert!(adopt(5, Off, Both, 6, 1, 2));
        assert!(!adopt(6, Off, Both, 5, 1, 2));
        assert!(!adopt(5, Both, Both, 5, 1, 2), "一样的不用采用");
        assert!(adopt(5, Here, Both, 5, 1, 2), "rev 相同：id 小的让");
        assert!(!adopt(5, Here, Both, 5, 3, 2));
    }

    #[test]
    fn mode_semantics() {
        use AudioMode::*;
        assert!(Both.send() && Both.play());
        assert!(There.send() && !There.play());
        assert!(!Here.send() && Here.play());
        assert!(!Off.send() && !Off.play());
        assert_eq!(Here.mirrored(), There);
        assert_eq!(Both.mirrored(), Both);
        assert_eq!(AudioMode::parse("whatever"), Off);
        for m in [Off, Here, There, Both] {
            assert_eq!(AudioMode::parse(m.as_str()), m);
        }
    }
}
