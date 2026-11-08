//! USB 键鼠线（SmartKMLink 这类两头各出一个 CDC 串口的对拷线）：把串口当成第二条通路，线插着就优先走线。
//!
//! 做法：不改会话层。本机在 127.0.0.1 上开一个“桥”端口，net 的 dialer 连它就等于经线连到对方；
//! 桥把这条 TCP 的字节流切成帧经串口送过去，对方的桥再连它自己的 127.0.0.1:cfg.port（kmlink 的监听端口）。
//! 握手、加密、会话替换等全部照旧（线上走的仍是会话层的密文）。鼠标位置 / 声音这类 UDP 包走 DGRAM 帧，不经 TCP。
//!
//! 线上帧：C5 3A | type(1) | id(4, LE) | len(2, LE) | payload(len ≤ 16 KiB) | crc32(4, LE，覆盖 type..payload)。
//!   type 低 7 位：HELLO / OPEN / DATA / CLOSE / DGRAM；最高位 = 这条连接是发送方开的（两边各自编号，互不冲突）。
//!   CRC 不对 / 长度离谱：跳过一个字节重新找帧头（插拔、对方重启时线里残留的半截数据就这样被丢掉）。
//! HELLO 每 250ms 一个，带本进程的 boot id。1.2 秒收不到 = 对方不在（kmlink 没开 / 卡死）：关掉经线的所有连接。
//!   对方 boot id 变了（重启了）同样全关。收到自己的 boot id = 线两头插在同一台电脑上，不算连上。
//! 串口读出错（拔线）：关串口、全关连接，每 2 秒重找一次。
//! 发送：一个写线程，先发 HELLO/OPEN/CLOSE/DGRAM（高优先），再发 DATA；DATA 队列至多 128 KiB，满了 TCP 那头就等着（背压）。
//! 设备：按 USB 的 VID/PID 在注册表 Enum\USB 下找它的 PortName；环境变量 KMLINK_CABLE=off 关掉，=COMx 指定串口。

use crate::hub::Hub;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 认得的线（USB 硬件 ID 前缀）。
const KNOWN: &[&str] = &["VID_0EA0&PID_7301"];
const SYNC: [u8; 2] = [0xC5, 0x3A];
const HDR: usize = 2 + 1 + 4 + 2;
const MAX_PAYLOAD: usize = 16 * 1024;
const T_HELLO: u8 = 1;
const T_OPEN: u8 = 2;
const T_DATA: u8 = 3;
const T_CLOSE: u8 = 4;
const T_DGRAM: u8 = 5;
const FLAG_SENDER_OPENED: u8 = 0x80;
const LINK_VER: u8 = 1;
const HELLO_EVERY: Duration = Duration::from_millis(250);
const PEER_DEAD: Duration = Duration::from_millis(1200);
const FIND_EVERY: Duration = Duration::from_secs(2);
/// TCP → 线：每次从本地连接读这么多（一个 DATA 帧），写线程两帧之间就能插进高优先的帧。
const PUMP_CHUNK: usize = 4096;
/// 写线程一次写给串口的量（凑够就写，高优先帧不受这个限制）。
const WRITE_BATCH: usize = 8 * 1024;
/// 排队等着上线的 DATA 至多这么多字节。
const LOW_MAX: usize = 128 * 1024;
/// 高优先队列（主要是鼠标位置 / 声音）至多这么多帧，多了丢新的（对方不在时不会积压）。
const HIGH_MAX: usize = 512;
/// 线 → 本地连接：写不进去这么久就关掉这条连接。
const LOCAL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- 帧（纯逻辑）

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC: [u32; 256] = crc_table();

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = !0u32;
    for p in parts {
        for &b in *p {
            c = CRC[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    !c
}

fn encode(t: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut v = Vec::with_capacity(HDR + payload.len() + 4);
    v.extend_from_slice(&SYNC);
    v.push(t);
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    v.extend_from_slice(payload);
    let c = crc32(&[&v[2..]]);
    v.extend_from_slice(&c.to_le_bytes());
    v
}

#[derive(Debug, PartialEq)]
struct RxFrame {
    t: u8,
    id: u32,
    payload: Vec<u8>,
}

/// b 开头是一个完整、校验正确的帧吗。
fn complete_at(b: &[u8]) -> bool {
    if b.len() < HDR || b[..2] != SYNC {
        return false;
    }
    let len = u16::from_le_bytes([b[7], b[8]]) as usize;
    let total = HDR + len + 4;
    len <= MAX_PAYLOAD
        && b.len() >= total
        && crc32(&[&b[2..HDR + len]]) == u32::from_le_bytes([b[total - 4], b[total - 3], b[total - 2], b[total - 1]])
}

/// 收帧：边收边找帧头，CRC 不对就跳过一个字节重找。
#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    pos: usize,
    /// 丢掉的字节数（排查用）。
    junk: u64,
}

impl Parser {
    fn push(&mut self, data: &[u8]) {
        if self.pos > 0 && (self.pos >= 64 * 1024 || self.pos == self.buf.len()) {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
        self.junk += n as u64;
    }

    fn next(&mut self) -> Option<RxFrame> {
        loop {
            let b = &self.buf[self.pos..];
            match b.windows(2).position(|w| w == SYNC) {
                Some(0) => {}
                Some(p) => self.skip(p),
                None => {
                    // 保留可能是半个帧头的最后一个字节
                    let keep = usize::from(b.last() == Some(&SYNC[0]));
                    self.skip(b.len() - keep);
                    return None;
                }
            }
            let b = &self.buf[self.pos..];
            if b.len() < HDR {
                return None;
            }
            let len = u16::from_le_bytes([b[7], b[8]]) as usize;
            if len > MAX_PAYLOAD {
                self.skip(1);
                continue;
            }
            let total = HDR + len + 4;
            if b.len() < total {
                // 还没收全。后面若已经有一个完整、校验对的帧，这个就是半截的（对方写到一半重启 / 写超时丢下的），扔掉
                match (1..b.len()).find(|&p| b[p..].starts_with(&SYNC) && complete_at(&b[p..])) {
                    Some(p) => {
                        self.skip(p);
                        continue;
                    }
                    None => return None,
                }
            }
            let want = u32::from_le_bytes([b[total - 4], b[total - 3], b[total - 2], b[total - 1]]);
            if crc32(&[&b[2..HDR + len]]) != want {
                self.skip(1);
                continue;
            }
            let f = RxFrame { t: b[2], id: u32::from_le_bytes([b[3], b[4], b[5], b[6]]), payload: b[HDR..HDR + len].to_vec() };
            self.pos += total;
            return Some(f);
        }
    }
}

// ---------------------------------------------------------------- 串口

mod port {
    use std::io;
    use windows::Win32::Devices::Communication::{COMMTIMEOUTS, PURGE_RXCLEAR, PURGE_TXCLEAR, PurgeComm, SetCommTimeouts};
    use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, ERROR_SEM_TIMEOUT, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, ReadFile, WriteFile,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::CreateEventW;
    use windows::core::PCWSTR;

    /// 重叠方式打开的串口：读线程和写线程可以同时各做各的（同步句柄上读会挡住写）。
    pub struct Port {
        h: HANDLE,
        rd_ev: HANDLE,
        wr_ev: HANDLE,
    }
    // SAFETY：句柄可以跨线程用；读只在读线程、写只在写线程（各用各的事件）
    unsafe impl Send for Port {}
    unsafe impl Sync for Port {}

    impl Port {
        pub fn open(name: &str) -> io::Result<Port> {
            let path = crate::sys::wide(&format!(r"\\.\{name}"));
            // SAFETY：path 以 0 结尾；返回的句柄由 Port 的 Drop 关
            unsafe {
                let h = CreateFileW(
                    PCWSTR(path.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )?;
                let ev = |h| CreateEventW(None, true, false, PCWSTR::null()).inspect_err(|_| {
                    let _ = CloseHandle(h);
                });
                let rd_ev = ev(h)?;
                let wr_ev = match ev(h) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = CloseHandle(rd_ev);
                        return Err(e.into());
                    }
                };
                let p = Port { h, rd_ev, wr_ev };
                // 读：有数据立刻返回，没有就最多等 500ms；写：1 秒写不完算超时（对方不收，线里的缓冲满了）
                let t = COMMTIMEOUTS {
                    ReadIntervalTimeout: u32::MAX,
                    ReadTotalTimeoutMultiplier: u32::MAX,
                    ReadTotalTimeoutConstant: 500,
                    WriteTotalTimeoutMultiplier: 0,
                    WriteTotalTimeoutConstant: 1000,
                };
                SetCommTimeouts(p.h, &t)?;
                let _ = PurgeComm(p.h, PURGE_RXCLEAR | PURGE_TXCLEAR);
                Ok(p)
            }
        }

        fn io(&self, ev: HANDLE, f: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>) -> io::Result<usize> {
            let mut ov = OVERLAPPED { hEvent: ev, ..Default::default() };
            let mut n = 0u32;
            // SAFETY：ov 在操作完成（GetOverlappedResult 等待返回）之前一直有效；缓冲区由调用方借出同样够久
            unsafe {
                // usbser 的读写超时报 ERROR_SEM_TIMEOUT（121），不算出错：返回已传的字节数
                let timeout = |e: &windows::core::Error| e.code() == ERROR_SEM_TIMEOUT.to_hresult();
                if let Err(e) = f(&mut ov) {
                    if timeout(&e) {
                        return Ok(0);
                    }
                    if e.code() != ERROR_IO_PENDING.to_hresult() {
                        return Err(e.into());
                    }
                }
                if let Err(e) = GetOverlappedResult(self.h, &ov, &mut n, true) {
                    if !timeout(&e) {
                        return Err(e.into());
                    }
                }
            }
            Ok(n as usize)
        }

        /// 读到一些就返回；500ms 没有数据返回 0。
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            // SAFETY：见 io
            self.io(self.rd_ev, |ov| unsafe { ReadFile(self.h, Some(buf), None, Some(ov)) })
        }

        /// 写一次；超时时返回已写的字节数（可能少于 buf.len()）。
        pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
            // SAFETY：见 io
            self.io(self.wr_ev, |ov| unsafe { WriteFile(self.h, Some(buf), None, Some(ov)) })
        }

        /// 让正在进行的读写立刻返回（关串口前）。
        pub fn cancel(&self) {
            // SAFETY：句柄有效
            unsafe {
                let _ = CancelIoEx(self.h, None);
            }
        }
    }

    impl Drop for Port {
        fn drop(&mut self) {
            // SAFETY：三个句柄都是本对象独占的
            unsafe {
                let _ = CloseHandle(self.h);
                let _ = CloseHandle(self.rd_ev);
                let _ = CloseHandle(self.wr_ev);
            }
        }
    }

    /// 注册表 Enum\USB 下这几类设备登记过的串口名（插着的、拔掉的都有，打开时再分辨）。
    pub fn candidates(known: &[&str]) -> Vec<String> {
        use windows::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
        };
        fn subkeys(k: HKEY) -> Vec<String> {
            let mut out = Vec::new();
            let mut i = 0u32;
            loop {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                // SAFETY：name/len 描述同一块缓冲
                let r = unsafe {
                    RegEnumKeyExW(k, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
                };
                if r.is_err() {
                    return out;
                }
                out.push(String::from_utf16_lossy(&name[..len as usize]));
                i += 1;
            }
        }
        fn open(parent: HKEY, sub: &str) -> Option<HKEY> {
            let w = crate::sys::wide(sub);
            let mut k = HKEY::default();
            // SAFETY：w 以 0 结尾
            unsafe { RegOpenKeyExW(parent, PCWSTR(w.as_ptr()), None, KEY_READ, &mut k) }.is_ok().then_some(k)
        }
        let mut ports = Vec::new();
        let Some(usb) = open(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Enum\USB") else { return ports };
        for dev in subkeys(usb) {
            let up = dev.to_ascii_uppercase();
            if !known.iter().any(|k| up.starts_with(k)) {
                continue;
            }
            let Some(dk) = open(usb, &dev) else { continue };
            for inst in subkeys(dk) {
                let sub = crate::sys::wide(&format!(r"{inst}\Device Parameters"));
                let val = crate::sys::wide("PortName");
                let mut buf = [0u16; 64];
                let mut len = (buf.len() * 2) as u32;
                // SAFETY：buf/len 描述同一块缓冲
                let r = unsafe {
                    RegGetValueW(
                        dk,
                        PCWSTR(sub.as_ptr()),
                        PCWSTR(val.as_ptr()),
                        RRF_RT_REG_SZ,
                        None,
                        Some(buf.as_mut_ptr().cast()),
                        Some(&mut len),
                    )
                };
                if r.is_ok() {
                    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    let p = String::from_utf16_lossy(&buf[..n]);
                    if p.to_ascii_uppercase().starts_with("COM") && !ports.contains(&p) {
                        ports.push(p);
                    }
                }
            }
            // SAFETY：dk 由 open 打开
            unsafe {
                let _ = RegCloseKey(dk);
            }
        }
        // SAFETY：usb 由 open 打开
        unsafe {
            let _ = RegCloseKey(usb);
        }
        ports
    }
}

// ---------------------------------------------------------------- 线

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    /// 本机开的连接（本机 dialer → 桥端口）。
    mine: bool,
    id: u32,
}

#[derive(Default)]
struct Link {
    /// 打开着的串口（名字）。
    port: Option<String>,
    /// 每次打开串口加一：写线程、桥连接看到它变了就收手。
    ep: u64,
    peer_boot: Option<u64>,
    heard: Option<Instant>,
    /// 对方 kmlink 在线（1.2 秒内收到过它的 HELLO）。
    alive: bool,
    /// 上次打开失败的原因（同样的不重复写日志）。
    last_err: String,
}

#[derive(Default)]
struct TxQ {
    ep: u64,
    high: VecDeque<Vec<u8>>,
    low: VecDeque<Vec<u8>>,
    low_bytes: usize,
}

type Sink = Box<dyn Fn(&[u8]) + Send + Sync>;
type Notify = Box<dyn Fn() + Send + Sync>;

pub struct Cable {
    hub: Arc<Hub>,
    boot: u64,
    link: Mutex<Link>,
    port: Mutex<Option<Arc<port::Port>>>,
    conns: Mutex<HashMap<Key, Arc<TcpStream>>>,
    /// 桥连到本机 kmlink 的连接，在 kmlink 那边看到的对端地址（= 这些连接的本地地址）。
    inbound: Mutex<HashSet<SocketAddr>>,
    listen: OnceLock<SocketAddr>,
    next_id: AtomicU32,
    tx: Mutex<TxQ>,
    tx_cv: Condvar,
    sink: OnceLock<Sink>,
    notify: OnceLock<Notify>,
}

/// 启动（KMLINK_CABLE=off 时不启动）。
pub fn start(hub: Arc<Hub>) -> Option<Arc<Cable>> {
    let forced = std::env::var("KMLINK_CABLE").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if forced.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("off")) {
        crate::log!("USB 线：已用 KMLINK_CABLE=off 关闭");
        return None;
    }
    let boot = u64::from_le_bytes(crate::crypto::random32()[..8].try_into().unwrap_or_default());
    let c = Arc::new(Cable {
        hub,
        boot,
        link: Mutex::new(Link::default()),
        port: Mutex::new(None),
        conns: Mutex::new(HashMap::new()),
        inbound: Mutex::new(HashSet::new()),
        listen: OnceLock::new(),
        next_id: AtomicU32::new(1),
        tx: Mutex::new(TxQ::default()),
        tx_cv: Condvar::new(),
        sink: OnceLock::new(),
        notify: OnceLock::new(),
    });
    match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(l) => {
            if let Ok(a) = l.local_addr() {
                let _ = c.listen.set(a);
            }
            let me = c.clone();
            spawn("cable-bridge", move || me.run_bridge(l));
        }
        Err(e) => {
            crate::log!("USB 线：本地桥端口打开失败：{e}，不用线");
            return None;
        }
    }
    let me = c.clone();
    spawn("cable", move || me.run_manager(forced));
    Some(c)
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(_) => true,
        Err(e) => {
            crate::log!("USB 线：创建线程 {name} 失败：{e}");
            false
        }
    }
}

impl Cable {
    /// 收到对方经线发来的 UDP 包（鼠标位置 / 声音）交给谁。
    pub fn set_sink(&self, f: Sink) {
        let _ = self.sink.set(f);
    }

    /// 线通了 / 断了时叫一声（net 的 dialer 立刻重拨）。
    pub fn set_notify(&self, f: Notify) {
        let _ = self.notify.set(f);
    }

    /// 线通着（串口开着、对方 kmlink 在线）。
    pub fn ready(&self) -> bool {
        let l = lock(&self.link);
        l.port.is_some() && l.alive
    }

    /// 线通着时，连这个地址 = 经线连到对方。
    pub fn dial_addr(&self) -> Option<SocketAddr> {
        if self.ready() { self.listen.get().copied() } else { None }
    }

    /// 这条会话（net 看到的对端地址）是经线的吗。
    pub fn owns(&self, peer: SocketAddr) -> bool {
        self.listen.get() == Some(&peer) || lock(&self.inbound).contains(&peer)
    }

    /// 经线发一个 UDP 包；线不通返回 false。
    pub fn send_dgram(&self, pkt: &[u8]) -> bool {
        if pkt.len() > MAX_PAYLOAD || !self.ready() {
            return false;
        }
        self.push_high(encode(T_DGRAM, 0, pkt))
    }

    fn wake_net(&self) {
        if let Some(f) = self.notify.get() {
            f();
        }
    }

    fn push_high(&self, f: Vec<u8>) -> bool {
        let mut q = lock(&self.tx);
        if q.high.len() >= HIGH_MAX {
            return false;
        }
        q.high.push_back(f);
        drop(q);
        self.tx_cv.notify_all();
        true
    }

    /// DATA 进低优先队列；满了就等（背压）。串口换了代 / 连接已关：false。
    fn push_low(&self, ep: u64, key: Key, f: Vec<u8>) -> bool {
        let mut q = lock(&self.tx);
        loop {
            if q.ep != ep || !lock(&self.conns).contains_key(&key) {
                return false;
            }
            if q.low_bytes < LOW_MAX {
                break;
            }
            q = self.tx_cv.wait_timeout(q, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
        q.low_bytes += f.len();
        q.low.push_back(f);
        drop(q);
        self.tx_cv.notify_all();
        true
    }

    fn frame_type(t: u8, key: Key) -> u8 {
        if key.mine { t | FLAG_SENDER_OPENED } else { t }
    }

    /// 关掉经线的全部连接（会话随之断开，net 改走网络重连）。
    fn close_all(&self) {
        let all: Vec<_> = lock(&self.conns).drain().map(|(_, s)| s).collect();
        lock(&self.inbound).clear();
        for s in all {
            let _ = s.shutdown(Shutdown::Both);
        }
        let mut q = lock(&self.tx);
        q.low.clear();
        q.low_bytes = 0;
        q.high.retain(|f| f.get(2).is_some_and(|t| t & 0x7F == T_HELLO));
        drop(q);
        self.tx_cv.notify_all();
    }

    fn close_conn(&self, key: Key, tell_peer: bool) {
        let Some(s) = lock(&self.conns).remove(&key) else { return };
        if let Ok(a) = s.local_addr() {
            lock(&self.inbound).remove(&a);
        }
        let _ = s.shutdown(Shutdown::Both);
        if tell_peer {
            self.push_high(encode(Self::frame_type(T_CLOSE, key), key.id, &[]));
        }
        self.tx_cv.notify_all();
    }

    fn epoch(&self) -> u64 {
        lock(&self.link).ep
    }

    // ------------------------------------------------------------ 线程

    /// 找串口、打开、发 HELLO、看对方在不在。
    fn run_manager(self: Arc<Self>, forced: Option<String>) {
        loop {
            if lock(&self.link).port.is_none() {
                if !self.try_open(forced.as_deref()) {
                    std::thread::sleep(FIND_EVERY);
                    continue;
                }
            }
            let mut hello = self.boot.to_le_bytes().to_vec();
            hello.push(LINK_VER);
            self.push_high(encode(T_HELLO, 0, &hello));
            let died = {
                let mut l = lock(&self.link);
                if l.alive && l.heard.is_none_or(|t| t.elapsed() > PEER_DEAD) {
                    l.alive = false;
                    true
                } else {
                    false
                }
            };
            if died {
                crate::log!("USB 线：对方 kmlink 没有回应，暂不走线");
                self.close_all();
                self.wake_net();
            }
            std::thread::sleep(HELLO_EVERY);
        }
    }

    fn try_open(self: &Arc<Self>, forced: Option<&str>) -> bool {
        let names = match forced {
            Some(p) => vec![p.to_string()],
            None => port::candidates(KNOWN),
        };
        let mut err = String::new();
        for name in &names {
            match port::Port::open(name) {
                Ok(p) => {
                    let p = Arc::new(p);
                    let ep = {
                        let mut l = lock(&self.link);
                        l.ep += 1;
                        l.port = Some(name.clone());
                        l.peer_boot = None;
                        l.heard = None;
                        l.alive = false;
                        l.last_err.clear();
                        l.ep
                    };
                    {
                        let mut q = lock(&self.tx);
                        *q = TxQ { ep, ..Default::default() };
                    }
                    *lock(&self.port) = Some(p.clone());
                    crate::log!("USB 线：已打开 {name}");
                    let (me, pr) = (self.clone(), p.clone());
                    spawn("cable-rx", move || me.run_reader(pr, ep));
                    let me = self.clone();
                    spawn("cable-tx", move || me.run_writer(p, ep));
                    return true;
                }
                Err(e) => err = format!("{name} 打不开：{e}"),
            }
        }
        // 没插线时注册表里也可能留着旧记录：打不开只在原因变了时记一次
        let mut l = lock(&self.link);
        if !err.is_empty() && l.last_err != err {
            crate::log!("USB 线：{err}");
            l.last_err = err;
        }
        false
    }

    /// 串口坏了（拔线）：关掉，全关连接，等 manager 重找。
    fn port_lost(&self, ep: u64, why: &str) {
        {
            let mut l = lock(&self.link);
            if l.ep != ep || l.port.is_none() {
                return;
            }
            crate::log!("USB 线：{} 断开（{why}）", l.port.as_deref().unwrap_or("?"));
            l.port = None;
            l.alive = false;
            l.peer_boot = None;
            l.ep += 1;
        }
        if let Some(p) = lock(&self.port).take() {
            p.cancel();
        }
        lock(&self.tx).ep = u64::MAX;
        self.close_all();
        self.wake_net();
    }

    fn run_reader(self: Arc<Self>, p: Arc<port::Port>, ep: u64) {
        // 鼠标位置在这里交给注入：一来就要处理
        crate::sys::boost_thread(true);
        let mut buf = vec![0u8; 64 * 1024];
        let mut parser = Parser::default();
        let why = loop {
            if self.epoch() != ep {
                return;
            }
            let n = match p.read(&mut buf) {
                Ok(n) => n,
                Err(e) => break e.to_string(),
            };
            if n == 0 {
                continue;
            }
            parser.push(&buf[..n]);
            while let Some(f) = parser.next() {
                self.on_frame(f, ep);
            }
        };
        self.port_lost(ep, &why);
    }

    fn on_frame(self: &Arc<Self>, f: RxFrame, ep: u64) {
        let kind = f.t & 0x7F;
        // 发送方开的连接 = 本机看来是对方开的
        let key = Key { mine: f.t & FLAG_SENDER_OPENED == 0, id: f.id };
        match kind {
            T_HELLO => {
                if f.payload.len() < 8 {
                    return;
                }
                let boot = u64::from_le_bytes(f.payload[..8].try_into().unwrap_or_default());
                if boot == self.boot {
                    let mut l = lock(&self.link);
                    if l.last_err != "loop" {
                        crate::log!("USB 线：线的两头插在同一台电脑上，不用");
                        l.last_err = "loop".into();
                    }
                    return;
                }
                let (restarted, up) = {
                    let mut l = lock(&self.link);
                    if l.ep != ep {
                        return;
                    }
                    let restarted = l.peer_boot.is_some_and(|b| b != boot);
                    l.peer_boot = Some(boot);
                    l.heard = Some(Instant::now());
                    let up = !l.alive || restarted;
                    l.alive = true;
                    (restarted, up)
                };
                if restarted {
                    crate::log!("USB 线：对方 kmlink 重启了");
                    self.close_all();
                }
                if up {
                    crate::log!("USB 线：已连上对方");
                    self.wake_net();
                }
            }
            T_OPEN if key.mine => {} // 对方不会以本机的名义开连接
            T_OPEN => {
                if !self.ready() {
                    self.push_high(encode(Self::frame_type(T_CLOSE, key), key.id, &[]));
                    return;
                }
                let port = self.hub.cfg().port;
                let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
                match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                    Ok(s) => {
                        if let Ok(a) = s.local_addr() {
                            lock(&self.inbound).insert(a);
                        }
                        self.adopt(key, s, ep);
                    }
                    Err(e) => {
                        crate::log!("USB 线：连本机端口 {port} 失败：{e}");
                        self.push_high(encode(Self::frame_type(T_CLOSE, key), key.id, &[]));
                    }
                }
            }
            T_DATA => {
                let s = lock(&self.conns).get(&key).cloned();
                match s {
                    Some(s) => {
                        if (&*s).write_all(&f.payload).is_err() {
                            self.close_conn(key, true);
                        }
                    }
                    None => {
                        self.push_high(encode(Self::frame_type(T_CLOSE, key), key.id, &[]));
                    }
                }
            }
            T_CLOSE => self.close_conn(key, false),
            T_DGRAM => {
                if let Some(sink) = self.sink.get() {
                    sink(&f.payload);
                }
            }
            _ => {}
        }
    }

    /// 登记一条经线的连接，起线程把它的数据泵到线上。
    fn adopt(self: &Arc<Self>, key: Key, s: TcpStream, ep: u64) {
        let _ = s.set_nodelay(true);
        let _ = s.set_write_timeout(Some(LOCAL_WRITE_TIMEOUT));
        let s = Arc::new(s);
        lock(&self.conns).insert(key, s.clone());
        let me = self.clone();
        if !spawn("cable-pump", move || me.pump(key, s, ep)) {
            self.close_conn(key, true);
        }
    }

    /// 本地连接 → 线。
    fn pump(self: Arc<Self>, key: Key, s: Arc<TcpStream>, ep: u64) {
        let mut buf = vec![0u8; PUMP_CHUNK];
        loop {
            let n = match (&*s).read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !self.push_low(ep, key, encode(Self::frame_type(T_DATA, key), key.id, &buf[..n])) {
                break;
            }
        }
        self.close_conn(key, true);
    }

    /// 本机 net 的 dialer 连进来 = 要经线连对方。
    fn run_bridge(self: Arc<Self>, l: TcpListener) {
        for c in l.incoming() {
            let Ok(s) = c else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            // 只给本机用
            if !s.peer_addr().is_ok_and(|a| a.ip().is_loopback()) || !self.ready() {
                continue;
            }
            let ep = self.epoch();
            let key = Key { mine: true, id: self.next_id.fetch_add(1, Ordering::Relaxed) };
            // 先登记再发 OPEN：对方回的 DATA 到了就有地方放
            let _ = s.set_nodelay(true);
            let _ = s.set_write_timeout(Some(LOCAL_WRITE_TIMEOUT));
            let s = Arc::new(s);
            lock(&self.conns).insert(key, s.clone());
            self.push_high(encode(Self::frame_type(T_OPEN, key), key.id, &[]));
            let me = self.clone();
            if !spawn("cable-pump", move || me.pump(key, s, ep)) {
                self.close_conn(key, true);
            }
        }
    }

    fn run_writer(self: Arc<Self>, p: Arc<port::Port>, ep: u64) {
        crate::sys::boost_thread(true);
        let mut out = Vec::with_capacity(WRITE_BATCH + MAX_PAYLOAD + 64);
        loop {
            out.clear();
            {
                let mut q = lock(&self.tx);
                loop {
                    if q.ep != ep {
                        return;
                    }
                    if !q.high.is_empty() || !q.low.is_empty() {
                        break;
                    }
                    q = self.tx_cv.wait_timeout(q, Duration::from_millis(500)).unwrap_or_else(|e| e.into_inner()).0;
                }
                while let Some(f) = q.high.pop_front() {
                    out.extend_from_slice(&f);
                }
                while out.len() < WRITE_BATCH {
                    let Some(f) = q.low.pop_front() else { break };
                    q.low_bytes -= f.len();
                    out.extend_from_slice(&f);
                }
            }
            self.tx_cv.notify_all();
            let mut done = 0;
            while done < out.len() {
                match p.write(&out[done..]) {
                    Ok(n) => done += n,
                    Err(e) => {
                        self.port_lost(ep, &e.to_string());
                        return;
                    }
                }
                if done < out.len() {
                    // 写超时：对方没在收。对方不在线就别硬塞了（残留的半帧对方会按 CRC 丢掉）
                    if self.epoch() != ep {
                        return;
                    }
                    if !lock(&self.link).alive {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(p: &mut Parser) -> Vec<RxFrame> {
        std::iter::from_fn(|| p.next()).collect()
    }

    #[test]
    fn roundtrip_and_split() {
        let a = encode(T_DATA | FLAG_SENDER_OPENED, 7, b"hello");
        let b = encode(T_DGRAM, 0, &[0xC5; 300]);
        let c = encode(T_CLOSE, 9, &[]);
        let wire: Vec<u8> = [a, b, c].concat();
        // 一次给完
        let mut p = Parser::default();
        p.push(&wire);
        let got = all(&mut p);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], RxFrame { t: T_DATA | FLAG_SENDER_OPENED, id: 7, payload: b"hello".to_vec() });
        assert_eq!(got[1].payload, vec![0xC5; 300]);
        assert_eq!(got[2].t, T_CLOSE);
        assert_eq!(p.junk, 0);
        // 一个字节一个字节地给
        let mut p = Parser::default();
        let mut got = Vec::new();
        for b in &wire {
            p.push(std::slice::from_ref(b));
            got.extend(all(&mut p));
        }
        assert_eq!(got.len(), 3);
        assert_eq!(p.junk, 0);
    }

    #[test]
    fn resync_after_garbage_and_corruption() {
        let good = encode(T_DGRAM, 0, b"move");
        let mut bad = encode(T_DATA, 1, b"broken!");
        bad[12] ^= 0xFF; // 改坏 payload
        let mut half = encode(T_DATA, 2, &[1; 100]);
        half.truncate(40); // 半截（对方重启前残留的）
        let wire: Vec<u8> = [vec![0x00, 0xC5, 0x11, 0x3A], bad, half, good.clone(), vec![0xC5]].concat();
        let mut p = Parser::default();
        p.push(&wire);
        let got = all(&mut p);
        assert_eq!(got, vec![RxFrame { t: T_DGRAM, id: 0, payload: b"move".to_vec() }]);
        assert!(p.junk > 0);
        // 结尾那个 C5 留着，下一帧照样能接上
        p.push(&good[1..]);
        assert_eq!(all(&mut p).len(), 1);
    }

    #[test]
    fn rejects_oversized_length() {
        let mut f = encode(T_DATA, 1, b"x");
        f[7] = 0xFF;
        f[8] = 0xFF; // 长度 65535 > 上限
        let good = encode(T_HELLO, 0, &[1, 2, 3, 4, 5, 6, 7, 8, 1]);
        let mut p = Parser::default();
        p.push(&[f, good].concat());
        let got = all(&mut p);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].t, T_HELLO);
    }

    #[test]
    fn crc_matches_reference() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
    }
}
