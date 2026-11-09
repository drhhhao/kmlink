//! A 端：清单表 + 按请求读文件发出。
//!
//! - 清单 id 是 128 位随机数，登记在“发给哪个对方实例”名下；对方重启了（实例 id 变了）立即作废。
//! - 至多 1 份当前 + 2 份旧的。本机剪贴板换了内容后当前的变“旧”：最后一次被请求后 10 分钟没人要才作废
//!   （正在粘贴的大文件不会被掐断），登记 24 小时后一律作废。
//! - 只服务清单里的项：index 越界、文件夹、offset / len 越界 → Err(BadReq)，一条连接累计 16 次就断开。
//!   每条连接未完成的请求累计 ≤ 8 MiB 且 ≤ 64 个，超了 = 协议错误，断开。
//! - 读盘在 files-io 线程：64 KiB 一块，句柄 ≤ 8 个（LRU）。打开时、之后每 1 MiB、最后一块之前核对
//!   大小和修改时间与清单一致，不一致回 Err(Changed)（不拼一个前后不一致的文件）。
//! - 不走线、键鼠正在跨屏用时，每块之间歇 250ms（约 256 KB/s），给键鼠让路。

use super::conn::{BLOCK, Conn, E_BADREQ, E_CHANGED, E_EXPIRED, E_GONE, E_READ, Frame};
use super::{Files, Manifest};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const KEEP_OLD: usize = 2;
const OLD_IDLE: Duration = Duration::from_secs(600);
const HARD_LIMIT: Duration = Duration::from_secs(24 * 3600);
const MAX_BAD: u32 = 16;
// 留足余量：B 端在途至多 48 个（fetch.rs MAX_INFLIGHT）、额度至多 8 MiB，取消在路上时 A 端还算着旧的
const MAX_OUT_BYTES: u64 = 16 << 20;
const MAX_OUT_N: u32 = 128;
const MAX_REQ: u32 = 1 << 20;
const HANDLES: usize = 8;
const CHECK_EVERY: u64 = 1 << 20;
const YIELD_PAUSE: Duration = Duration::from_millis(250);
const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

struct Offer {
    mid: [u8; 16],
    peer_id: u64,
    man: Arc<Manifest>,
    paths: Arc<Vec<PathBuf>>,
    current: bool,
    created: Instant,
    last_req: Instant,
    /// 压缩好的清单（第一次被要时做）。
    z: Option<Arc<Vec<u8>>>,
}

impl Offer {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) > HARD_LIMIT || (!self.current && now.saturating_duration_since(self.last_req) > OLD_IDLE)
    }
}

struct Job {
    conn: Arc<Conn>,
    rid: u32,
    mid: [u8; 16],
    path: PathBuf,
    size: u64,
    mtime: u64,
    offset: u64,
    len: u32,
}

#[derive(Default)]
struct ConnStat {
    id: u64,
    bad: u32,
    out_bytes: u64,
    out_n: u32,
}

#[derive(Default)]
struct TState {
    offers: Vec<Offer>,
    jobs: VecDeque<Job>,
    stats: Vec<ConnStat>,
    /// files-io 正在做的 (连接 id, rid)，以及它是否已被取消。
    doing: Option<(u64, u32, [u8; 16])>,
    doing_cancelled: bool,
    /// 最近一次收到对方的请求（文件连接闲置判断用）。
    last_act: Option<Instant>,
}

impl TState {
    fn stat(&mut self, id: u64) -> &mut ConnStat {
        if let Some(i) = self.stats.iter().position(|s| s.id == id) {
            return &mut self.stats[i];
        }
        self.stats.push(ConnStat { id, ..Default::default() });
        self.stats.last_mut().unwrap()
    }

    fn done(&mut self, id: u64, len: u32) {
        let s = self.stat(id);
        s.out_bytes = s.out_bytes.saturating_sub(len as u64);
        s.out_n = s.out_n.saturating_sub(1);
    }
}

#[derive(Default)]
pub struct Table {
    st: Mutex<TState>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn start(f: Arc<Files>) {
    super::spawn("files-io", move || run_io(f));
}

impl Table {
    /// 本机复制了文件：登记清单，返回要发给对方的摘要。之前的“当前”清单变旧。
    pub fn register(&self, man: Manifest, paths: Vec<PathBuf>, peer_id: u64) -> super::Summary {
        let mid = super::new_mid();
        let sum = man.summary(&mid);
        let now = Instant::now();
        let mut st = lock(&self.st);
        st.offers.retain(|o| !o.expired(now));
        for o in st.offers.iter_mut().filter(|o| o.current) {
            o.current = false;
            o.last_req = now;
        }
        while st.offers.len() > KEEP_OLD {
            st.offers.remove(0);
        }
        st.offers.push(Offer { mid, peer_id, man: Arc::new(man), paths: Arc::new(paths), current: true, created: now, last_req: now, z: None });
        sum
    }

    /// 本机剪贴板换成了别的内容：当前清单变旧（10 分钟没人要才作废）。
    pub fn retire(&self) {
        let now = Instant::now();
        for o in lock(&self.st).offers.iter_mut().filter(|o| o.current) {
            o.current = false;
            o.last_req = now;
        }
    }

    pub fn peer_restarted(&self) {
        let mut st = lock(&self.st);
        st.offers.clear();
        st.jobs.clear();
        st.doing_cancelled = true;
    }

    pub fn conn_lost(&self, id: u64) {
        let mut st = lock(&self.st);
        st.jobs.retain(|j| j.conn.id != id);
        st.stats.retain(|s| s.id != id);
        if st.doing.is_some_and(|d| d.0 == id) {
            st.doing_cancelled = true;
        }
    }

    /// 正在服务，或 within 内对方还要过数据。
    pub fn busy(&self, within: Duration) -> bool {
        let st = lock(&self.st);
        !st.jobs.is_empty() || st.doing.is_some() || st.last_act.is_some_and(|t| t.elapsed() < within)
    }

    /// 本机又复制了同一批文件（摘要没变，不再发给对方）：那份清单重新算“当前”，别让它 10 分钟后作废。
    pub fn revive(&self, digest: u64) {
        let mut st = lock(&self.st);
        if let Some(o) = st.offers.iter_mut().rev().find(|o| o.man.digest() == digest) {
            o.current = true;
            o.last_req = Instant::now();
        }
    }

    fn find(&self, st: &mut TState, mid: &[u8; 16], peer: u64) -> Option<usize> {
        let now = Instant::now();
        let i = st.offers.iter().position(|o| &o.mid == mid && (o.peer_id == peer || o.peer_id == 0))?;
        if st.offers[i].expired(now) {
            st.offers.remove(i);
            return None;
        }
        st.offers[i].last_req = now;
        st.last_act = Some(now);
        Some(i)
    }

    pub fn on_list_req(&self, c: &Arc<Conn>, rid: u32, mid: [u8; 16]) {
        let peer = super::get().map(|f| f.peer_id()).unwrap_or(0);
        let z = {
            let mut st = lock(&self.st);
            let Some(i) = self.find(&mut st, &mid, peer) else {
                drop(st);
                c.send_ctl(Frame::Err { rid, code: E_EXPIRED });
                return;
            };
            let o = &mut st.offers[i];
            if o.z.is_none() {
                let json = serde_json::to_vec(&*o.man).unwrap_or_default();
                o.z = Some(Arc::new(miniz_oxide::deflate::compress_to_vec(&json, 6)));
            }
            o.z.clone().unwrap_or_default()
        };
        let chunks: Vec<&[u8]> = if z.is_empty() { vec![&[][..]] } else { z.chunks(BLOCK).collect() };
        let n = chunks.len();
        for (k, part) in chunks.into_iter().enumerate() {
            c.send_ctl(Frame::List { rid, seq: k as u16, last: k + 1 == n, z: part });
        }
    }

    /// 收到 Req。返回 false = 对方违规，断开这条文件连接。
    pub fn on_req(&self, c: &Arc<Conn>, rid: u32, mid: [u8; 16], index: u32, offset: u64, len: u32) -> bool {
        let peer = super::get().map(|f| f.peer_id()).unwrap_or(0);
        let mut st = lock(&self.st);
        let Some(i) = self.find(&mut st, &mid, peer) else {
            drop(st);
            c.send_ctl(Frame::Err { rid, code: E_EXPIRED });
            return true;
        };
        let (man, paths) = (st.offers[i].man.clone(), st.offers[i].paths.clone());
        let item = man.items.get(index as usize);
        let ok = item.is_some_and(|it| !it.d && len > 0 && len <= MAX_REQ && offset <= it.s && offset + len as u64 <= it.s);
        if !ok {
            let s = st.stat(c.id);
            s.bad += 1;
            let alive = s.bad < MAX_BAD;
            drop(st);
            c.send_ctl(Frame::Err { rid, code: E_BADREQ });
            return alive;
        }
        let s = st.stat(c.id);
        if s.out_bytes + len as u64 > MAX_OUT_BYTES || s.out_n >= MAX_OUT_N {
            return false;
        }
        s.out_bytes += len as u64;
        s.out_n += 1;
        let it = &man.items[index as usize];
        st.jobs.push_back(Job { conn: c.clone(), rid, mid, path: paths[index as usize].clone(), size: it.s, mtime: it.m, offset, len });
        drop(st);
        self.cv.notify_all();
        true
    }

    pub fn on_cancel(&self, c: &Arc<Conn>, rid: u32, mid: [u8; 16]) {
        let mut st = lock(&self.st);
        let hit = |j_conn: u64, j_rid: u32, j_mid: &[u8; 16]| j_conn == c.id && j_mid == &mid && (rid == 0 || j_rid == rid);
        let mut freed = Vec::new();
        st.jobs.retain(|j| {
            let h = hit(j.conn.id, j.rid, &j.mid);
            if h {
                freed.push(j.len);
            }
            !h
        });
        for len in freed {
            st.done(c.id, len);
        }
        if st.doing.is_some_and(|(id, r, m)| hit(id, r, &m)) {
            st.doing_cancelled = true;
        }
    }
}

/// 打开着的文件（LRU）。
struct Handles(VecDeque<(PathBuf, File)>);

impl Handles {
    fn get(&mut self, p: &PathBuf) -> std::io::Result<&mut File> {
        if let Some(i) = self.0.iter().position(|(q, _)| q == p) {
            let h = self.0.remove(i).unwrap();
            self.0.push_back(h);
        } else {
            let f = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_ALL).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(p)?;
            if self.0.len() >= HANDLES {
                self.0.pop_front();
            }
            self.0.push_back((p.clone(), f));
        }
        Ok(&mut self.0.back_mut().unwrap().1)
    }

    fn forget(&mut self, p: &PathBuf) {
        self.0.retain(|(q, _)| q != p);
    }
}

/// 句柄上的大小和修改时间还和清单一样吗。
fn same(f: &File, size: u64, mtime: u64) -> bool {
    f.metadata().is_ok_and(|m| !m.is_dir() && m.len() == size && m.last_write_time() == mtime)
}

fn run_io(files: Arc<Files>) {
    let t = &files.serve;
    let mut hs = Handles(VecDeque::new());
    let mut idle_since = Instant::now();
    loop {
        let job = {
            let mut st = lock(&t.st);
            loop {
                if let Some(j) = st.jobs.pop_front() {
                    st.doing = Some((j.conn.id, j.rid, j.mid));
                    st.doing_cancelled = false;
                    break j;
                }
                // 闲了一会儿就关掉所有句柄（别一直占着用户的文件）
                if !hs.0.is_empty() && idle_since.elapsed() > Duration::from_secs(5) {
                    hs.0.clear();
                }
                st = t.cv.wait_timeout(st, Duration::from_secs(1)).unwrap_or_else(|e| e.into_inner()).0;
            }
        };
        let code = serve(t, &mut hs, &job);
        if let Some(code) = code {
            hs.forget(&job.path);
            job.conn.send_ctl(Frame::Err { rid: job.rid, code });
        }
        let mut st = lock(&t.st);
        st.doing = None;
        st.done(job.conn.id, job.len);
        idle_since = Instant::now();
    }
}

/// 发一个请求的数据。Some(code) = 要回 Err。
fn serve(t: &Table, hs: &mut Handles, j: &Job) -> Option<u8> {
    let cancelled = || lock(&t.st).doing_cancelled;
    let f = match hs.get(&j.path) {
        Ok(f) => f,
        Err(e) => {
            crate::log!("文件：打不开 {}：{e}", j.path.display());
            return Some(if e.kind() == std::io::ErrorKind::NotFound { E_GONE } else { E_READ });
        }
    };
    if !same(f, j.size, j.mtime) {
        crate::log!("文件：{} 已经变了（大小或修改时间和复制时不同）", j.path.display());
        return Some(E_CHANGED);
    }
    if f.seek(SeekFrom::Start(j.offset)).is_err() {
        return Some(E_READ);
    }
    let mut buf = vec![0u8; BLOCK];
    let mut off = j.offset;
    let end = j.offset + j.len as u64;
    let mut since_check = 0u64;
    while off < end {
        if cancelled() {
            return None;
        }
        let n = ((end - off) as usize).min(BLOCK);
        if let Err(e) = f.read_exact(&mut buf[..n]) {
            crate::log!("文件：读 {} 失败：{e}", j.path.display());
            return Some(if e.kind() == std::io::ErrorKind::UnexpectedEof { E_CHANGED } else { E_READ });
        }
        since_check += n as u64;
        let last = off + n as u64 >= end;
        if (since_check >= CHECK_EVERY || last) && !same(f, j.size, j.mtime) {
            crate::log!("文件：{} 在传的时候被改了", j.path.display());
            return Some(E_CHANGED);
        }
        if since_check >= CHECK_EVERY {
            since_check = 0;
        }
        if !j.conn.cable && crate::net::input_active() {
            std::thread::sleep(YIELD_PAUSE);
        }
        if !j.conn.send_data(Frame::Data { rid: j.rid, offset: off, bytes: &buf[..n] }, &cancelled) {
            return None;
        }
        off += n as u64;
    }
    None
}
