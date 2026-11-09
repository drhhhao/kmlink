//! 文件连接：KMLF 握手（同一把配对密钥、密钥域分开）+ 会话票据 + 二进制帧。每个主会话至多一条，按请求号多路复用，
//! 两个方向都能用（谁复制了文件谁就是服务方）。
//!
//! 帧（明文，加密分帧同主连接：u32 长度 ‖ ChaCha20-Poly1305 密文）：第 1 字节是类型。
//!   Hello  ver u8 | id u64 | ticket [32]          握手后第一帧，客户端先发
//!   Ping / Pong  t u32                            每秒一次（6 秒收不到任何帧就断）
//!   ListReq rid u32 | mid [16]                    B→A 要完整清单
//!   List   rid u32 | seq u16 | last u8 | z..      A→B 压缩清单分片（≤ 64 KiB）
//!   Req    rid u32 | mid [16] | index u32 | offset u64 | len u32     B→A
//!   Data   rid u32 | offset u64 | bytes..        A→B（≤ 64 KiB，同一 rid 内按 offset 连续）
//!   Err    rid u32 | code u8                      A→B
//!   Cancel rid u32 | mid [16]                     B→A（rid = 0：这个清单的全部请求）
//! 认证之后的任何读写错误都按“断线，可续传”处理，只记日志。

use super::{Files, SessInfo};
use crate::crypto::{Opener, Sealer};
use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const VER: u8 = 1;
/// Data 帧里数据的上限。
pub const BLOCK: usize = 64 * 1024;
/// 读帧（密文）上限：一个 Data 帧 + 头 + 标签。
const MAX_CT: usize = BLOCK + 64 + 16;
const READ_TIMEOUT: Duration = Duration::from_secs(6);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const PING_EVERY: Duration = Duration::from_secs(1);
/// 写线程前面排着的 Data 至多这么多字节（多了服务方等着：背压）。
const DATA_Q_MAX: usize = 256 * 1024;
const SNDBUF: i32 = 64 * 1024;

const F_HELLO: u8 = 1;
const F_PING: u8 = 2;
const F_PONG: u8 = 3;
const F_LISTREQ: u8 = 4;
const F_LIST: u8 = 5;
const F_REQ: u8 = 6;
const F_DATA: u8 = 7;
const F_ERR: u8 = 9;
const F_CANCEL: u8 = 10;

/// Err 帧的原因。
pub const E_GONE: u8 = 1;
pub const E_READ: u8 = 3;
pub const E_CHANGED: u8 = 4;
pub const E_EXPIRED: u8 = 5;
pub const E_BADREQ: u8 = 6;

#[derive(Debug, PartialEq)]
pub enum Frame<'a> {
    Hello { ver: u8, id: u64, ticket: [u8; 32] },
    Ping(u32),
    Pong(u32),
    ListReq { rid: u32, mid: [u8; 16] },
    List { rid: u32, seq: u16, last: bool, z: &'a [u8] },
    Req { rid: u32, mid: [u8; 16], index: u32, offset: u64, len: u32 },
    Data { rid: u32, offset: u64, bytes: &'a [u8] },
    Err { rid: u32, code: u8 },
    Cancel { rid: u32, mid: [u8; 16] },
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        if self.0.len() < N {
            return None;
        }
        let (a, b) = self.0.split_at(N);
        self.0 = b;
        a.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
}

impl<'a> Frame<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(48);
        match self {
            Frame::Hello { ver, id, ticket } => {
                v.push(F_HELLO);
                v.push(*ver);
                v.extend_from_slice(&id.to_le_bytes());
                v.extend_from_slice(ticket);
            }
            Frame::Ping(t) | Frame::Pong(t) => {
                v.push(if matches!(self, Frame::Ping(_)) { F_PING } else { F_PONG });
                v.extend_from_slice(&t.to_le_bytes());
            }
            Frame::ListReq { rid, mid } => {
                v.push(F_LISTREQ);
                v.extend_from_slice(&rid.to_le_bytes());
                v.extend_from_slice(mid);
            }
            Frame::List { rid, seq, last, z } => {
                v.reserve(z.len());
                v.push(F_LIST);
                v.extend_from_slice(&rid.to_le_bytes());
                v.extend_from_slice(&seq.to_le_bytes());
                v.push(*last as u8);
                v.extend_from_slice(z);
            }
            Frame::Req { rid, mid, index, offset, len } => {
                v.push(F_REQ);
                v.extend_from_slice(&rid.to_le_bytes());
                v.extend_from_slice(mid);
                v.extend_from_slice(&index.to_le_bytes());
                v.extend_from_slice(&offset.to_le_bytes());
                v.extend_from_slice(&len.to_le_bytes());
            }
            Frame::Data { rid, offset, bytes } => {
                v.reserve(bytes.len());
                v.push(F_DATA);
                v.extend_from_slice(&rid.to_le_bytes());
                v.extend_from_slice(&offset.to_le_bytes());
                v.extend_from_slice(bytes);
            }
            Frame::Err { rid, code } => {
                v.push(F_ERR);
                v.extend_from_slice(&rid.to_le_bytes());
                v.push(*code);
            }
            Frame::Cancel { rid, mid } => {
                v.push(F_CANCEL);
                v.extend_from_slice(&rid.to_le_bytes());
                v.extend_from_slice(mid);
            }
        }
        v
    }

    pub fn decode(b: &'a [u8]) -> Option<Frame<'a>> {
        let (&t, rest) = b.split_first()?;
        let mut r = Rd(rest);
        let f = match t {
            F_HELLO => Frame::Hello { ver: r.u8()?, id: r.u64()?, ticket: r.take()? },
            F_PING => Frame::Ping(r.u32()?),
            F_PONG => Frame::Pong(r.u32()?),
            F_LISTREQ => Frame::ListReq { rid: r.u32()?, mid: r.take()? },
            F_LIST => {
                let (rid, seq, last) = (r.u32()?, r.u16()?, r.u8()?);
                return (r.0.len() <= BLOCK).then_some(Frame::List { rid, seq, last: last != 0, z: r.0 });
            }
            F_REQ => Frame::Req { rid: r.u32()?, mid: r.take()?, index: r.u32()?, offset: r.u64()?, len: r.u32()? },
            F_DATA => {
                let (rid, offset) = (r.u32()?, r.u64()?);
                return (!r.0.is_empty() && r.0.len() <= BLOCK).then_some(Frame::Data { rid, offset, bytes: r.0 });
            }
            F_ERR => Frame::Err { rid: r.u32()?, code: r.u8()? },
            F_CANCEL => Frame::Cancel { rid: r.u32()?, mid: r.take()? },
            _ => return None,
        };
        r.0.is_empty().then_some(f)
    }
}

#[derive(Default)]
struct TxQ {
    ctl: VecDeque<Vec<u8>>,
    data: VecDeque<Vec<u8>>,
    data_bytes: usize,
}

pub struct Conn {
    pub id: u64,
    /// 经 USB 线（走线时不限速）。
    pub cable: bool,
    sock: TcpStream,
    dead: AtomicBool,
    q: Mutex<TxQ>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn mono_us() -> u32 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u32
}

impl Conn {
    /// 拨号（本机是主会话的客户端）。
    pub fn dial(files: Arc<Files>, addr: SocketAddr, sess: &SessInfo, cable: bool) -> io::Result<Arc<Conn>> {
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        Self::handshake(files, s, true, None, sess, cable)
    }

    /// 握手 + 互换 Hello（核对对方 id 和本会话的票据），成功后起读写线程。
    pub fn handshake(files: Arc<Files>, mut s: TcpStream, client: bool, pre: Option<[u8; 4]>, sess: &SessInfo, cable: bool) -> io::Result<Arc<Conn>> {
        let psk = files.hub.cfg().key_bytes().ok_or_else(|| bad("密钥无效"))?;
        let _ = s.set_nodelay(true);
        crate::net::set_sndbuf(&s, SNDBUF);
        let (mut tx, mut rx) = crate::net::file_handshake(&mut s, &psk, client, pre)?;
        s.set_read_timeout(Some(HELLO_TIMEOUT))?;
        s.set_write_timeout(Some(HELLO_TIMEOUT))?;
        let me = crate::proto::instance_id();
        let hello = Frame::Hello { ver: VER, id: me, ticket: sess.ticket }.encode();
        if client {
            crate::net::write_frame(&mut s, &mut tx, &hello)?;
        }
        let first = crate::net::read_frame(&mut s, &mut rx, 256)?;
        match Frame::decode(&first) {
            Some(Frame::Hello { ver, id, ticket }) => {
                if ver != VER {
                    return Err(bad("文件连接版本不同"));
                }
                if id == me || id != sess.peer_id || ticket != sess.ticket {
                    return Err(bad("不是当前会话的对方"));
                }
            }
            _ => return Err(bad("第一帧不是 Hello")),
        }
        if !client {
            crate::net::write_frame(&mut s, &mut tx, &hello)?;
        }
        s.set_read_timeout(Some(READ_TIMEOUT))?;
        s.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let c = Arc::new(Conn {
            id: files.conn_id(),
            cable,
            sock: s.try_clone()?,
            dead: AtomicBool::new(false),
            q: Mutex::new(TxQ::default()),
            cv: Condvar::new(),
        });
        let w = s.try_clone()?;
        let (c1, f1) = (c.clone(), files.clone());
        if !super::spawn("fconn-tx", move || c1.run_writer(w, tx, f1)) {
            return Err(io::Error::other("创建线程失败"));
        }
        let (c2, f2) = (c.clone(), files);
        if !super::spawn("fconn-rx", move || c2.run_reader(s, rx, f2)) {
            c.close();
            return Err(io::Error::other("创建线程失败"));
        }
        Ok(c)
    }

    pub fn alive(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if !self.dead.swap(true, Ordering::Relaxed) {
            let _ = self.sock.shutdown(Shutdown::Both);
        }
        self.cv.notify_all();
    }

    /// 控制帧（请求、取消、错误、清单）：不限量，排在数据前面。
    pub fn send_ctl(&self, f: Frame) -> bool {
        if !self.alive() {
            return false;
        }
        lock(&self.q).ctl.push_back(f.encode());
        self.cv.notify_all();
        true
    }

    /// 数据帧：前面排着的数据多了就等（背压）。连接断了 / stop() 为真：false。
    pub fn send_data(&self, f: Frame, stop: &dyn Fn() -> bool) -> bool {
        let b = f.encode();
        let mut q = lock(&self.q);
        loop {
            if !self.alive() || stop() {
                return false;
            }
            if q.data_bytes < DATA_Q_MAX {
                break;
            }
            q = self.cv.wait_timeout(q, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner()).0;
        }
        q.data_bytes += b.len();
        q.data.push_back(b);
        drop(q);
        self.cv.notify_all();
        true
    }

    fn run_writer(self: Arc<Self>, mut s: TcpStream, mut tx: Sealer, files: Arc<Files>) {
        let mut last_ping = Instant::now();
        let why = loop {
            let f = {
                let mut q = lock(&self.q);
                loop {
                    if !self.alive() {
                        break None;
                    }
                    if let Some(f) = q.ctl.pop_front() {
                        break Some(f);
                    }
                    if let Some(f) = q.data.pop_front() {
                        q.data_bytes -= f.len();
                        break Some(f);
                    }
                    if last_ping.elapsed() >= PING_EVERY {
                        break Some(Frame::Ping(mono_us()).encode());
                    }
                    q = self.cv.wait_timeout(q, PING_EVERY).unwrap_or_else(|e| e.into_inner()).0;
                }
            };
            self.cv.notify_all();
            let Some(f) = f else { break "已关闭".to_string() };
            if f.first() == Some(&F_PING) {
                last_ping = Instant::now();
            }
            if let Err(e) = crate::net::write_frame(&mut s, &mut tx, &f) {
                break format!("发送失败：{e}");
            }
        };
        self.close();
        files.conn_lost(self.id, &why);
    }

    fn run_reader(self: Arc<Self>, mut s: TcpStream, mut rx: Opener, files: Arc<Files>) {
        let why = loop {
            let plain = match crate::net::read_frame(&mut s, &mut rx, MAX_CT) {
                Ok(p) => p,
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => break "6 秒没有收到数据".to_string(),
                Err(e) => break e.to_string(),
            };
            let Some(f) = Frame::decode(&plain) else { break "帧不合法".to_string() };
            let ok = match f {
                Frame::Ping(t) => self.send_ctl(Frame::Pong(t)),
                Frame::Pong(_) => true,
                Frame::Hello { .. } => false,
                Frame::ListReq { rid, mid } => {
                    files.serve.on_list_req(&self, rid, mid);
                    true
                }
                Frame::Req { rid, mid, index, offset, len } => files.serve.on_req(&self, rid, mid, index, offset, len),
                Frame::Cancel { rid, mid } => {
                    files.serve.on_cancel(&self, rid, mid);
                    true
                }
                Frame::List { rid, seq, last, z } => files.fetch.on_list(rid, seq, last, z),
                Frame::Data { rid, offset, bytes } => files.fetch.on_data(rid, offset, bytes),
                Frame::Err { rid, code } => {
                    files.fetch.on_err(rid, code);
                    true
                }
            };
            if !ok {
                break "对方的帧不合规".to_string();
            }
            if !self.alive() {
                break "已关闭".to_string();
            }
        };
        self.close();
        files.conn_lost(self.id, &why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let data = vec![7u8; BLOCK];
        let all = [
            Frame::Hello { ver: 1, id: 9, ticket: [3; 32] },
            Frame::Ping(5),
            Frame::Pong(6),
            Frame::ListReq { rid: 1, mid: [2; 16] },
            Frame::List { rid: 1, seq: 2, last: true, z: b"zz" },
            Frame::Req { rid: 3, mid: [4; 16], index: 5, offset: 1 << 40, len: 1 << 20 },
            Frame::Data { rid: 3, offset: 77, bytes: &data },
            Frame::Err { rid: 3, code: E_CHANGED },
            Frame::Cancel { rid: 0, mid: [9; 16] },
        ];
        for f in all {
            let b = f.encode();
            assert_eq!(Frame::decode(&b), Some(f));
        }
        // 多余字节、截短、空数据、超长数据都不认
        let mut b = Frame::Err { rid: 1, code: 1 }.encode();
        b.push(0);
        assert_eq!(Frame::decode(&b), None);
        let b = Frame::Req { rid: 3, mid: [4; 16], index: 5, offset: 1, len: 1 }.encode();
        assert_eq!(Frame::decode(&b[..b.len() - 1]), None);
        assert_eq!(Frame::decode(&Frame::Data { rid: 1, offset: 0, bytes: &[] }.encode()), None);
        let big = vec![0u8; BLOCK + 1];
        assert_eq!(Frame::decode(&Frame::Data { rid: 1, offset: 0, bytes: &big }.encode()), None);
        assert_eq!(Frame::decode(&[99]), None);
    }
}
