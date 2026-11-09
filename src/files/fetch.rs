//! B 端：拉清单、按额度要数据、续传、Read 语义、粘贴进度。
//!
//! - 额度（全局，所有清单、所有流共用）：走线 8 MiB，局域网 1 MiB。发 Req 之前先占额度，数据被读走 / 请求取消时归还。
//!   文件连接的读线程收到的 Data 一定有地方放（只放进内存），从不等资源管理器、从不写盘。
//! - 每个文件一个缓冲：[base, base + buf.len()) 是收齐的连续数据，后面跟着至多 2 个在途请求。
//!   读的位置跳了（Seek、资源管理器“重试”从 0 开始）就作废缓冲、取消在途请求，从新位置重新要。
//! - 跨文件预读：读第 i 个文件时，额度用了不到一半就给后面 64 个文件各先要开头一块（≤ 256 KiB），
//!   1 万个小文件不再每个等一个往返。
//! - 续传：文件连接断了，在途请求作废、已收到的留着；连上后从缓冲末尾接着要。断开超过 30 秒、
//!   或连着却 60 秒毫无进展，正在等的 Read 失败。
//! - Read 不满不回：凑满或真到文件尾才返回；出错一律返回错误，绝不用“少给几个字节”表达错误。

use super::conn::{Conn, E_CHANGED, E_EXPIRED, Frame};
use super::{Manifest, RESUME_WAIT, STALL, Summary};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const CREDIT_CABLE: u64 = 8 << 20;
const CREDIT_LAN: u64 = 1 << 20;
const REQ_CABLE: u64 = 1 << 20;
const REQ_LAN: u64 = 256 << 10;
const AHEAD_CABLE: u64 = 2 << 20;
const AHEAD_LAN: u64 = 512 << 10;
const READAHEAD_FILES: u32 = 64;
const READAHEAD_HEAD: u64 = 256 << 10;
/// 所有清单、所有文件加起来至多这么多个在途请求（A 端一条连接至多容 128 个，见 serve.rs MAX_OUT_N）。
const MAX_INFLIGHT: usize = 48;
const LIST_WAIT: Duration = Duration::from_secs(30);
/// 压缩后的清单至多这么大（解压后另有 16 MiB 上限）。
const MAX_LIST_Z: usize = 16 << 20;
/// 预读了却一直没人读的缓冲：这么久后丢掉。
const READAHEAD_IDLE: Duration = Duration::from_secs(60);
/// 最后一个流结束后这么久没有新流：这次粘贴算结束（弹“已收到”/“没传完”）。
const PASTE_END: Duration = Duration::from_secs(3);
/// 粘贴超过这么久才弹“接收中”。
const PROGRESS_AFTER: Duration = Duration::from_secs(1);
const KEEP_REMOTES: usize = 3;

/// Read 失败的原因（ole.rs 换成 HRESULT）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadErr {
    /// 断线太久、对方报读不了 / 没了：资源管理器弹“重试 / 跳过”。
    Net,
    /// A 端文件变了、清单作废、对方重启。
    Changed,
    /// 流已放弃、kmlink 退出。
    Abort,
    /// 用户在灵动岛上取消了这次粘贴。
    Cancelled,
}

/// 粘贴进度提示（交给 desktop 弹灵动岛）。
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    /// 已读字节、总字节（每 250ms 一次，灵动岛据此平滑地走进度、估剩余时间）
    Progress { read: u64, total: u64 },
    Done(u32),
    Failed,
    Cancelled,
}

/// 收到并校验过的对方清单。
pub struct Remote {
    pub mid: [u8; 16],
    pub man: Manifest,
    /// FILEGROUPDESCRIPTORW 的字节（只建一份）。
    pub desc: Vec<u8>,
    pub summary: Summary,
}

struct Inflight {
    rid: u32,
    off: u64,
    len: u64,
    got: u64,
    /// 不是排在最前的请求先到的数据（排到最前时并进 buf）。
    data: Vec<u8>,
}

struct FBuf {
    base: u64,
    buf: VecDeque<u8>,
    reqs: VecDeque<Inflight>,
    err: Option<u8>,
    used: Instant,
    /// 只是预读的（还没有流读过它）。
    ahead: bool,
}

impl FBuf {
    fn new(base: u64) -> FBuf {
        FBuf { base, buf: VecDeque::new(), reqs: VecDeque::new(), err: None, used: Instant::now(), ahead: false }
    }
    /// 已收到 + 在途覆盖到的末尾。
    fn covered(&self) -> u64 {
        self.reqs.back().map(|r| r.off + r.len).unwrap_or(self.base + self.buf.len() as u64)
    }
}

#[derive(Default)]
struct Paste {
    open: u32,
    started: Option<Instant>,
    last_close: Option<Instant>,
    ok_files: u32,
    failed: bool,
    /// 用户取消了：之后这次粘贴的 Read 一律返回“已取消”。
    cancelled: bool,
    read: u64,
    last_read: Option<u64>,
}

struct RState {
    remote: Arc<Remote>,
    seq: u64,
    dead: Option<ReadErr>,
    files: HashMap<u32, FBuf>,
    /// 已预取到本机临时目录的文件：流直接读本地。
    local: HashMap<u32, PathBuf>,
    paste: Paste,
}

struct ListRx {
    z: Vec<u8>,
    next: u16,
    done: Option<Result<Vec<u8>, u8>>,
}

#[derive(Default)]
struct FState {
    remotes: HashMap<[u8; 16], RState>,
    reqs: HashMap<u32, ([u8; 16], u32)>,
    lists: HashMap<u32, ListRx>,
    next_rid: u32,
    next_seq: u64,
    credit: u64,
    lost_at: Option<Instant>,
    exiting: bool,
}

impl FState {
    fn rid(&mut self) -> u32 {
        self.next_rid = self.next_rid.wrapping_add(1).max(1);
        self.next_rid
    }
}

#[derive(Default)]
pub struct Fetcher {
    st: Mutex<FState>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 作废一个文件的在途请求：归还额度，返回要发 Cancel 的 rid。
fn drop_inflight(f: &mut FBuf, reqs: &mut HashMap<u32, ([u8; 16], u32)>, credit: &mut u64) -> Vec<u32> {
    let mut rids = Vec::with_capacity(f.reqs.len());
    for (k, r) in f.reqs.drain(..).enumerate() {
        // 排在最前的那个已收到的部分在 buf 里（随 buf 归还）
        *credit = credit.saturating_sub(if k == 0 { r.len - r.got } else { r.len });
        reqs.remove(&r.rid);
        rids.push(r.rid);
    }
    rids
}

/// 作废整个缓冲，从 p 重新开始。
fn reset(f: &mut FBuf, p: u64, reqs: &mut HashMap<u32, ([u8; 16], u32)>, credit: &mut u64) -> Vec<u32> {
    let rids = drop_inflight(f, reqs, credit);
    *credit = credit.saturating_sub(f.buf.len() as u64);
    f.buf.clear();
    f.base = p;
    f.err = None;
    rids
}

fn cancel(conn: Option<&Arc<Conn>>, mid: &[u8; 16], rids: &[u32]) {
    if let Some(c) = conn {
        for &rid in rids {
            c.send_ctl(Frame::Cancel { rid, mid: *mid });
        }
    }
}

fn map_err(code: u8) -> ReadErr {
    if code == E_CHANGED || code == E_EXPIRED { ReadErr::Changed } else { ReadErr::Net }
}

struct Sizes {
    credit: u64,
    req: u64,
    ahead: u64,
}

fn sizes(c: &Conn) -> Sizes {
    if c.cable {
        Sizes { credit: CREDIT_CABLE, req: REQ_CABLE, ahead: AHEAD_CABLE }
    } else {
        Sizes { credit: CREDIT_LAN, req: REQ_LAN, ahead: AHEAD_LAN }
    }
}

/// 在额度内给一个文件补请求：从已覆盖的末尾起，至多领先 limit 字节、至多 2 个在途。
#[allow(clippy::too_many_arguments)]
fn issue(f: &mut FBuf, mid: &[u8; 16], idx: u32, size: u64, limit: u64, cap: u64, sz: &Sizes, st_rid: &mut dyn FnMut() -> u32, reqs: &mut HashMap<u32, ([u8; 16], u32)>, credit: &mut u64, c: &Conn) {
    loop {
        if reqs.len() >= MAX_INFLIGHT {
            return;
        }
        let end = f.covered();
        if end >= size || end - f.base >= limit || f.reqs.len() >= 2 {
            return;
        }
        let len = (size - end).min(sz.req).min(limit - (end - f.base));
        if len == 0 || *credit + len > cap {
            return;
        }
        let rid = st_rid();
        reqs.insert(rid, (*mid, idx));
        f.reqs.push_back(Inflight { rid, off: end, len, got: 0, data: Vec::new() });
        *credit += len;
        c.send_ctl(Frame::Req { rid, mid: *mid, index: idx, offset: end, len: len as u32 });
    }
}

impl Fetcher {
    fn files() -> Option<&'static Arc<super::Files>> {
        super::get()
    }

    // ------------------------------------------------------------ 拉清单

    /// 拉完整清单、校验、登记。阻塞（在工作线程里调）。Err = 原因（日志用）。
    pub fn get_list(&self, s: &Summary) -> Result<Arc<Remote>, &'static str> {
        let files = Self::files().ok_or("没启动")?;
        let mid = super::parse_mid(&s.mid).ok_or("清单 id 不对")?;
        let until = Instant::now() + LIST_WAIT;
        let z = loop {
            if Instant::now() > until {
                return Err("连不上文件连接");
            }
            let Some(c) = files.conn() else {
                let st = lock(&self.st);
                if st.exiting {
                    return Err("退出");
                }
                let _ = self.cv.wait_timeout(st, Duration::from_millis(200));
                continue;
            };
            let rid = {
                let mut st = lock(&self.st);
                let rid = st.rid();
                st.lists.insert(rid, ListRx { z: Vec::new(), next: 0, done: None });
                rid
            };
            c.send_ctl(Frame::ListReq { rid, mid });
            let mut st = lock(&self.st);
            let r = loop {
                if let Some(d) = st.lists.get_mut(&rid).and_then(|l| l.done.take()) {
                    break Some(d);
                }
                if st.exiting || !c.alive() || Instant::now() > until {
                    break None;
                }
                st = self.cv.wait_timeout(st, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
            };
            st.lists.remove(&rid);
            match r {
                Some(Ok(z)) => break z,
                Some(Err(_)) => return Err("清单已作废"),
                None if st.exiting => return Err("退出"),
                None => continue,
            }
        };
        let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(&z, super::MAX_LIST_RAW).map_err(|_| "清单解不开")?;
        let man: Manifest = serde_json::from_slice(&raw).map_err(|_| "清单格式不对")?;
        drop(raw);
        super::validate(&man)?;
        if man.count != s.count || man.bytes != s.bytes || man.digest() != s.digest {
            return Err("清单和摘要对不上");
        }
        let desc = super::descriptor(&man);
        let remote = Arc::new(Remote { mid, man, desc, summary: s.clone() });
        let mut st = lock(&self.st);
        st.next_seq += 1;
        let seq = st.next_seq;
        // 只留最近几份（正在粘贴的不动）
        while st.remotes.len() >= KEEP_REMOTES {
            let Some(old) = st.remotes.iter().filter(|(_, r)| r.paste.open == 0).min_by_key(|(_, r)| r.seq).map(|(k, _)| *k) else { break };
            self.forget_locked(&mut st, &old);
        }
        st.remotes.insert(mid, RState { remote: remote.clone(), seq, dead: None, files: HashMap::new(), local: HashMap::new(), paste: Paste::default() });
        Ok(remote)
    }

    fn forget_locked(&self, st: &mut FState, mid: &[u8; 16]) {
        let Some(mut rs) = st.remotes.remove(mid) else { return };
        let conn = Self::files().and_then(|f| f.current_conn());
        let FState { reqs, credit, .. } = st;
        for f in rs.files.values_mut() {
            let rids = reset(f, 0, reqs, credit);
            cancel(conn.as_ref(), mid, &rids);
        }
    }

    pub fn on_list(&self, rid: u32, seq: u16, last: bool, z: &[u8]) -> bool {
        let mut st = lock(&self.st);
        let Some(l) = st.lists.get_mut(&rid) else { return true };
        if l.done.is_some() || seq != l.next || l.z.len() + z.len() > MAX_LIST_Z {
            l.done = Some(Err(0));
        } else {
            l.z.extend_from_slice(z);
            l.next = l.next.wrapping_add(1);
            if last {
                l.done = Some(Ok(std::mem::take(&mut l.z)));
            }
        }
        drop(st);
        self.cv.notify_all();
        true
    }

    // ------------------------------------------------------------ 数据

    /// 收到 Data。false = 对方违规（偏移不对、超长），断开文件连接（当作断线，走续传）。
    pub fn on_data(&self, rid: u32, offset: u64, bytes: &[u8]) -> bool {
        let mut st = lock(&self.st);
        let Some(&(mid, idx)) = st.reqs.get(&rid) else { return true }; // 已取消的迟到数据
        let FState { remotes, reqs, .. } = &mut *st;
        let Some(f) = remotes.get_mut(&mid).and_then(|r| r.files.get_mut(&idx)) else {
            reqs.remove(&rid);
            return true;
        };
        let Some(k) = f.reqs.iter().position(|r| r.rid == rid) else {
            reqs.remove(&rid);
            return true;
        };
        let r = &mut f.reqs[k];
        if offset != r.off + r.got || r.got + bytes.len() as u64 > r.len {
            return false;
        }
        r.got += bytes.len() as u64;
        if k == 0 {
            f.buf.extend(bytes);
        } else {
            r.data.extend_from_slice(bytes);
        }
        while f.reqs.front().is_some_and(|r| r.got == r.len) {
            let done = f.reqs.pop_front().unwrap();
            reqs.remove(&done.rid);
            if let Some(n) = f.reqs.front_mut() {
                f.buf.extend(std::mem::take(&mut n.data));
            }
        }
        drop(st);
        self.cv.notify_all();
        true
    }

    pub fn on_err(&self, rid: u32, code: u8) {
        let mut st = lock(&self.st);
        if let Some(l) = st.lists.get_mut(&rid) {
            l.done = Some(Err(code));
        } else if let Some((mid, idx)) = st.reqs.get(&rid).copied() {
            let FState { remotes, reqs, credit, .. } = &mut *st;
            if let Some(rs) = remotes.get_mut(&mid) {
                if code == E_EXPIRED {
                    rs.dead = Some(ReadErr::Changed);
                }
                if let Some(f) = rs.files.get_mut(&idx) {
                    f.err = Some(code);
                    drop_inflight(f, reqs, credit);
                }
            }
            reqs.remove(&rid);
        }
        drop(st);
        self.cv.notify_all();
    }

    /// 换连接 / 连接没了：在本锁内做 f（f 里可以拿 link 锁：顺序 fetch → link）。f 返回 true 就作废全部在途请求
    /// （已收到的留着，之后从缓冲末尾接着要）。持着本锁时没人能发新请求，作废的一定都属于已死 / 被换掉的连接。
    /// up = 换上了新连接（清掉断线时刻）。
    pub fn on_conn_change(&self, f: impl FnOnce() -> bool, up: bool) {
        let mut st = lock(&self.st);
        let FState { remotes, reqs, credit, lost_at, .. } = &mut *st;
        if f() {
            for rs in remotes.values_mut() {
                for fb in rs.files.values_mut() {
                    drop_inflight(fb, reqs, credit);
                }
            }
            reqs.clear();
            if !up {
                lost_at.get_or_insert(Instant::now());
            }
        }
        if up {
            *lost_at = None;
        }
        drop(st);
        self.cv.notify_all();
    }

    /// 对方重启：它之前的清单全部作废，正在等的 Read 立即失败。
    pub fn peer_restarted(&self) {
        let mut st = lock(&self.st);
        for rs in st.remotes.values_mut() {
            rs.dead = Some(ReadErr::Changed);
        }
        drop(st);
        self.cv.notify_all();
    }

    pub fn shutdown(&self) {
        lock(&self.st).exiting = true;
        self.cv.notify_all();
    }

    pub fn busy(&self) -> bool {
        let st = lock(&self.st);
        !st.lists.is_empty() || !st.reqs.is_empty() || st.remotes.values().any(|r| r.paste.open > 0)
    }

    pub fn alive(&self, mid: &[u8; 16]) -> bool {
        lock(&self.st).remotes.get(mid).is_some_and(|r| r.dead.is_none())
    }

    /// 预取完一个文件：之后的流直接读本地。
    pub fn set_local(&self, mid: &[u8; 16], idx: u32, p: PathBuf) {
        let mut st = lock(&self.st);
        let FState { remotes, reqs, credit, .. } = &mut *st;
        if let Some(rs) = remotes.get_mut(mid) {
            if let Some(mut f) = rs.files.remove(&idx) {
                reset(&mut f, 0, reqs, credit);
            }
            rs.local.insert(idx, p);
        }
    }

    pub fn has_readers(&self, mid: &[u8; 16]) -> bool {
        lock(&self.st).remotes.get(mid).is_some_and(|r| r.paste.open > 0)
    }

    // ------------------------------------------------------------ 流

    pub fn stream_open(&self, mid: &[u8; 16]) {
        let mut st = lock(&self.st);
        if let Some(rs) = st.remotes.get_mut(mid) {
            rs.paste.open += 1;
            rs.paste.started.get_or_insert(Instant::now());
        }
    }

    /// 流被释放：读完了（done）还是半路放弃（用户取消 / 跳过）。
    pub fn stream_close(&self, mid: &[u8; 16], idx: u32, done: bool) {
        let mut st = lock(&self.st);
        let conn = Self::files().and_then(|f| f.current_conn());
        let FState { remotes, reqs, credit, .. } = &mut *st;
        let Some(rs) = remotes.get_mut(mid) else { return };
        rs.paste.open = rs.paste.open.saturating_sub(1);
        rs.paste.last_close = Some(Instant::now());
        if done {
            rs.paste.ok_files += 1;
        }
        // 读完的也放掉缓冲（不然每个读完的文件都留着一块内存，直到清单被挤掉）
        if let Some(mut f) = rs.files.remove(&idx) {
            let rids = reset(&mut f, 0, reqs, credit);
            cancel(conn.as_ref(), mid, &rids);
        }
    }

    /// 预取半路放弃一个文件：丢掉它的缓冲和在途请求（不算粘贴进度）。
    pub fn drop_file(&self, mid: &[u8; 16], idx: u32) {
        let mut st = lock(&self.st);
        let conn = Self::files().and_then(|f| f.current_conn());
        let FState { remotes, reqs, credit, .. } = &mut *st;
        let Some(rs) = remotes.get_mut(mid) else { return };
        if let Some(mut f) = rs.files.remove(&idx) {
            let rids = reset(&mut f, 0, reqs, credit);
            cancel(conn.as_ref(), mid, &rids);
        }
    }

    /// 预取失败、临时目录要删了：之后的流改回走网络。
    pub fn clear_local(&self, mid: &[u8; 16]) {
        if let Some(rs) = lock(&self.st).remotes.get_mut(mid) {
            rs.local.clear();
        }
    }

    /// 读 [pos, pos + out.len())：凑满或到文件尾才返回。quiet = 预取（不算进粘贴进度，失败也不算这次粘贴失败）。
    pub fn read(&self, r: &Remote, idx: u32, pos: u64, out: &mut [u8], quiet: bool) -> Result<usize, ReadErr> {
        let res = self.read_inner(r, idx, pos, out, quiet);
        if res.is_err() && res != Err(ReadErr::Cancelled) && !quiet {
            if let Some(rs) = lock(&self.st).remotes.get_mut(&r.mid) {
                rs.paste.failed = true;
            }
        }
        res
    }

    fn read_inner(&self, r: &Remote, idx: u32, pos: u64, out: &mut [u8], quiet: bool) -> Result<usize, ReadErr> {
        let size = r.man.items.get(idx as usize).map(|i| i.s).ok_or(ReadErr::Abort)?;
        if pos >= size || out.is_empty() {
            return Ok(0);
        }
        let want = (out.len() as u64).min(size - pos) as usize;
        let files = Self::files().ok_or(ReadErr::Abort)?;
        let mut filled = 0usize;
        let started = Instant::now();
        let mut progress = started;
        let mut first = true;
        let mut st = lock(&self.st);
        loop {
            if st.exiting {
                return Err(ReadErr::Abort);
            }
            let local = {
                let Some(rs) = st.remotes.get(&r.mid) else { return Err(ReadErr::Changed) };
                if let Some(e) = rs.dead {
                    return Err(e);
                }
                if rs.paste.cancelled && !quiet {
                    return Err(ReadErr::Cancelled);
                }
                rs.local.get(&idx).cloned()
            };
            if let Some(p) = local {
                drop(st);
                if let Some(n) = read_local(&p, pos + filled as u64, &mut out[filled..want]) {
                    return Ok(filled + n);
                }
                // 本地副本读不了（被删了）：改回走网络
                st = lock(&self.st);
                if let Some(rs) = st.remotes.get_mut(&r.mid) {
                    rs.local.remove(&idx);
                }
                continue;
            }
            let conn = files.conn();
            let p = pos + filled as u64;
            let mut cancels = Vec::new();
            let mut done = false;
            let mut err = None;
            {
                let FState { remotes, reqs, credit, next_rid, lost_at, .. } = &mut *st;
                let rs = remotes.get_mut(&r.mid).unwrap();
                let f = rs.files.entry(idx).or_insert_with(|| FBuf::new(p));
                f.used = Instant::now();
                // 每次 Read 进来先清掉上次的错误（资源管理器点“重试”要真的再问 A，而不是复读旧错误）
                if first || f.ahead {
                    f.err = None;
                    first = false;
                }
                f.ahead = false;
                let end = f.base + f.buf.len() as u64;
                if p < f.base || p > end {
                    cancels = reset(f, p, reqs, credit);
                    // 先发取消，再发新请求（A 端先放掉旧请求占的名额）
                    cancel(conn.as_ref(), &r.mid, &cancels);
                    cancels.clear();
                } else if p > f.base {
                    let k = (p - f.base) as usize;
                    f.buf.drain(..k);
                    f.base = p;
                    *credit = credit.saturating_sub(k as u64);
                }
                let take = f.buf.len().min(want - filled);
                if take > 0 {
                    let (a, b) = f.buf.as_slices();
                    let na = a.len().min(take);
                    out[filled..filled + na].copy_from_slice(&a[..na]);
                    out[filled + na..filled + take].copy_from_slice(&b[..take - na]);
                    f.buf.drain(..take);
                    f.base += take as u64;
                    *credit = credit.saturating_sub(take as u64);
                    filled += take;
                    progress = Instant::now();
                    if !quiet {
                        rs.paste.read += take as u64;
                    }
                }
                if filled == want {
                    done = true;
                } else if let Some(code) = f.err {
                    err = Some(map_err(code));
                }
                match &conn {
                    Some(c) => {
                        *lost_at = None;
                        let sz = sizes(c);
                        let mut rid = || {
                            *next_rid = next_rid.wrapping_add(1).max(1);
                            *next_rid
                        };
                        if err.is_none() {
                            // 这个文件：领先 ahead；读完了也顺手把后面的预读补上
                            issue(f, &r.mid, idx, size, sz.ahead, sz.credit, &sz, &mut rid, reqs, credit, c);
                            if !quiet {
                                for j in idx + 1..(idx + 1 + READAHEAD_FILES).min(r.man.items.len() as u32) {
                                    if *credit >= sz.credit / 2 {
                                        break;
                                    }
                                    let it = &r.man.items[j as usize];
                                    if it.d || it.s == 0 || rs.local.contains_key(&j) {
                                        continue;
                                    }
                                    let g = rs.files.entry(j).or_insert_with(|| {
                                        let mut b = FBuf::new(0);
                                        b.ahead = true;
                                        b
                                    });
                                    if g.ahead && g.err.is_none() && g.covered() == 0 {
                                        issue(g, &r.mid, j, it.s, READAHEAD_HEAD, sz.credit / 2, &sz, &mut rid, reqs, credit, c);
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // 断开时刻早于这次 Read 的（上次闲置断开留下的），从这次 Read 起算
                        let t = (*lost_at.get_or_insert(Instant::now())).max(started);
                        if !done && err.is_none() && t.elapsed() > RESUME_WAIT {
                            err = Some(ReadErr::Net);
                        }
                    }
                }
            }
            if !cancels.is_empty() {
                cancel(conn.as_ref(), &r.mid, &cancels);
            }
            if done {
                return Ok(filled);
            }
            if let Some(e) = err {
                return Err(e);
            }
            if progress.elapsed() > STALL {
                return Err(ReadErr::Net);
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    // ------------------------------------------------------------ 定时：进度提示、清理

    /// 用户在灵动岛上点了取消：正在进行的粘贴之后的 Read 都返回“已取消”。返回有没有正在进行的粘贴。
    pub fn cancel_paste(&self) -> bool {
        let mut st = lock(&self.st);
        let mut any = false;
        for rs in st.remotes.values_mut().filter(|r| r.paste.started.is_some()) {
            rs.paste.cancelled = true;
            any = true;
        }
        drop(st);
        self.cv.notify_all();
        any
    }

    /// 每 250ms 调一次：返回要弹的提示（谁的、什么）。
    pub fn tick(&self) -> Vec<(Arc<Remote>, Note)> {
        let mut out = Vec::new();
        let conn = Self::files().and_then(|f| f.current_conn());
        let mut st = lock(&self.st);
        let FState { remotes, reqs, credit, .. } = &mut *st;
        for rs in remotes.values_mut() {
            // 预读了没人读的：丢掉
            let mid = rs.remote.mid;
            let stale: Vec<u32> = rs.files.iter().filter(|(_, f)| f.ahead && f.used.elapsed() > READAHEAD_IDLE).map(|(k, _)| *k).collect();
            for k in stale {
                if let Some(mut f) = rs.files.remove(&k) {
                    let rids = reset(&mut f, 0, reqs, credit);
                    cancel(conn.as_ref(), &mid, &rids);
                }
            }
            let p = &mut rs.paste;
            let Some(t0) = p.started else { continue };
            // 每个文件都读完了：马上算结束（不必再等 PASTE_END，灵动岛不用满格干等）
            let all_done = p.open == 0 && p.ok_files as usize >= rs.remote.man.files() && !p.cancelled;
            if !all_done && (p.open > 0 || p.last_close.is_none_or(|t| t.elapsed() < PASTE_END)) {
                if t0.elapsed() >= PROGRESS_AFTER && p.open > 0 && p.last_read != Some(p.read) {
                    p.last_read = Some(p.read);
                    out.push((rs.remote.clone(), Note::Progress { read: p.read, total: rs.remote.man.bytes }));
                }
                continue;
            }
            let note = if p.cancelled {
                Some(Note::Cancelled)
            } else if p.failed {
                Some(Note::Failed)
            } else if p.ok_files > 0 {
                Some(Note::Done(p.ok_files))
            } else {
                None // 只有取消：用户自己取消的，不弹
            };
            crate::log!(
                "文件：一次粘贴结束：完成 {} 个文件，{}，读了 {} KB，用时 {:.1} 秒",
                p.ok_files,
                if p.failed { "有失败" } else { "没有失败" },
                p.read / 1024,
                t0.elapsed().as_secs_f32() - PASTE_END.as_secs_f32()
            );
            *p = Paste::default();
            if let Some(n) = note {
                out.push((rs.remote.clone(), n));
            }
        }
        out
    }
}

fn read_local(p: &PathBuf, pos: u64, out: &mut [u8]) -> Option<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    f.seek(SeekFrom::Start(pos)).ok()?;
    let mut n = 0;
    while n < out.len() {
        match f.read(&mut out[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return None,
        }
    }
    Some(n)
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Remote {{ mid: {}, items: {}, bytes: {} }}", super::hex(&self.mid), self.man.items.len(), self.man.bytes)
    }
}
