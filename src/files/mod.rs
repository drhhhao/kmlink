//! 复制粘贴文件（规格：设计\方案\复制粘贴文件-实现.md）。
//!
//! A 台复制文件：只把摘要（Msg::ClipFiles）发给对方，不传内容。B 台收到后经“文件连接”拉完整清单、校验，
//! 在剪贴板上放一个“虚拟文件”对象（FileGroupDescriptorW + FileContents）。B 上粘贴时，资源管理器逐个要文件流，
//! 流被读的时候才经文件连接向 A 要数据。插着线、总共 ≤ 50 MB 时，B 另外把文件先取到临时目录，
//! 取完再给剪贴板加上 CF_HDROP（微信 / QQ 只认真文件路径）。
//!
//! 模块：
//! - mod.rs：常量、清单（Manifest）、名字清洗、摘要、全局状态（会话、文件连接、拨号）。
//! - conn.rs：文件连接（KMLF 握手、票据、帧、读写线程）。
//! - scan.rs：A 端展开文件夹。serve.rs：A 端清单表、按请求读文件发出。
//! - fetch.rs：B 端拉清单、额度、请求、续传、Read 语义、粘贴进度。
//! - ole.rs：B 端 clip-ole（STA）线程、MTA 里的 IDataObject / IStream。prefetch.rs：预取到临时目录、清理。

pub mod conn;
pub mod fetch;
pub mod ole;
pub mod prefetch;
pub mod scan;
pub mod serve;

use crate::cable::Cable;
use crate::hub::Hub;
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 清单至多这么多项（含文件夹）。
pub const MAX_ITEMS: usize = 10_000;
/// 清单 JSON 解压后至多这么大。
pub const MAX_LIST_RAW: usize = 16 << 20;
/// 相对路径（UTF-16 单元，不含结尾 0）/ 单个组件的上限：FILEDESCRIPTORW.cFileName 是 [u16; 260]。
pub const MAX_PATH_UNITS: usize = 259;
pub const MAX_COMPONENT: usize = 255;
/// 断线后等这么久重连续传，超过就让正在等的 Read 失败。
pub const RESUME_WAIT: Duration = Duration::from_secs(30);
/// 连着但这么久毫无进展：Read 失败。
pub const STALL: Duration = Duration::from_secs(60);
/// 预取到临时目录（微信 / QQ 兜底）的总大小上限（用户定的 50 MB），且只在走线时预取。
pub const PREFETCH_MAX: u64 = 50 * 1000 * 1000;
/// 主会话断了这么久没有同一对方的新会话：关文件连接。
const ORPHAN_AFTER: Duration = Duration::from_secs(30);
/// 文件连接这么久没人用就关。
const IDLE_CLOSE: Duration = Duration::from_secs(300);

/// 只取描述符里这几个属性（只读、隐藏、存档、目录）。
pub const KEEP_ATTRS: u32 = 0x1 | 0x2 | 0x20 | 0x10;
pub const ATTR_DIR: u32 = 0x10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(_) => true,
        Err(e) => {
            crate::log!("文件：创建线程 {name} 失败：{e}");
            false
        }
    }
}

// ---------------------------------------------------------------- 清单

/// 清单里的一项。p = 相对路径（'\' 分隔，第一层是选中的项）；s = 大小；m = LastWriteTime（FILETIME）；
/// a = 属性（只留 KEEP_ATTRS）；d = 是文件夹。顺序：文件夹在它里面的东西之前。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub p: String,
    pub s: u64,
    pub m: u64,
    pub a: u32,
    pub d: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub items: Vec<Item>,
    pub bytes: u64,
    pub count: u32,
}

impl Manifest {
    pub fn new(items: Vec<Item>) -> Manifest {
        let bytes = items.iter().map(|i| i.s).sum();
        let count = items.len() as u32;
        Manifest { items, bytes, count }
    }

    /// 第一层的项。
    pub fn tops(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| !i.p.contains('\\'))
    }

    /// 文件（不含文件夹）的个数。
    pub fn files(&self) -> usize {
        self.items.iter().filter(|i| !i.d).count()
    }

    /// 回声摘要：按相对路径（小写）排序后的 (路径, 大小, 修改时间)。B 上把粘出来的同一批文件再复制，摘要相同。
    pub fn digest(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut v: Vec<(String, u64, u64)> = self.items.iter().map(|i| (i.p.to_lowercase(), if i.d { 0 } else { i.s }, if i.d { 0 } else { i.m })).collect();
        v.sort();
        let mut h = std::hash::DefaultHasher::new();
        "kmlink files".hash(&mut h);
        v.hash(&mut h);
        h.finish()
    }

    pub fn summary(&self, mid: &[u8; 16]) -> Summary {
        let tops: Vec<&Item> = self.tops().collect();
        Summary {
            mid: hex(mid),
            count: self.count,
            bytes: self.bytes,
            tops: tops.len() as u32,
            dirs: tops.iter().filter(|i| i.d).count() as u32,
            top: if tops.len() == 1 { tops[0].p.chars().take(64).collect() } else { String::new() },
            digest: self.digest(),
        }
    }
}

/// 摘要（Msg::ClipFiles 的内容）。
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub mid: String,
    pub count: u32,
    pub bytes: u64,
    pub tops: u32,
    pub dirs: u32,
    pub top: String,
    pub digest: u64,
}

impl Summary {
    pub fn to_msg(&self) -> crate::proto::Msg {
        crate::proto::Msg::ClipFiles {
            mid: self.mid.clone(),
            count: self.count,
            bytes: self.bytes,
            tops: self.tops,
            dirs: self.dirs,
            top: self.top.clone(),
            digest: self.digest,
        }
    }

    /// 提示里“已复制”后面的话：一项写名字，全是文件写“N 个文件”，全是文件夹写“N 个文件夹”，混着写“N 项”。
    pub fn what(&self) -> String {
        if self.tops == 1 && !self.top.is_empty() {
            return self.top.clone();
        }
        if self.dirs == 0 {
            format!("{} 个文件", self.tops)
        } else if self.dirs == self.tops {
            format!("{} 个文件夹", self.tops)
        } else {
            format!("{} 项", self.tops)
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn parse_mid(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn new_mid() -> [u8; 16] {
    let r = crate::crypto::random32();
    let mut m = [0u8; 16];
    m.copy_from_slice(&r[..16]);
    m
}

// ---------------------------------------------------------------- 名字清洗（纯函数）

fn bad_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
        || (c as u32) < 0x20
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

fn reserved(comp: &str) -> bool {
    let base = comp.split('.').next().unwrap_or("").trim_end_matches(' ').to_uppercase();
    const FIXED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    if FIXED.contains(&base.as_str()) {
        return true;
    }
    for p in ["COM", "LPT"] {
        if let Some(rest) = base.strip_prefix(p) {
            let mut it = rest.chars();
            if let (Some(c), None) = (it.next(), it.next()) {
                if c.is_ascii_digit() || matches!(c, '¹' | '²' | '³') {
                    return true;
                }
            }
        }
    }
    false
}

/// 一个路径组件是否合格。
pub fn component_ok(c: &str) -> Result<(), &'static str> {
    if c.is_empty() || c == "." || c == ".." {
        return Err("名字不支持");
    }
    if c.chars().any(bad_char) || c.ends_with('.') || c.ends_with(' ') || reserved(c) {
        return Err("名字不支持");
    }
    if c.encode_utf16().count() > MAX_COMPONENT {
        return Err("路径太长");
    }
    Ok(())
}

/// 整份清单是否合格（A 发出前、B 收到后各查一遍）。任何一项不合格 = 整份不要。
pub fn validate(m: &Manifest) -> Result<(), &'static str> {
    if m.items.is_empty() {
        return Err("没有文件");
    }
    if m.items.len() > MAX_ITEMS || m.count as usize != m.items.len() {
        return Err("文件太多");
    }
    let mut total: u64 = 0;
    let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::with_capacity(m.items.len());
    for it in &m.items {
        if it.p.encode_utf16().count() > MAX_PATH_UNITS {
            return Err("路径太长");
        }
        let comps: Vec<&str> = it.p.split('\\').collect();
        for c in &comps {
            component_ok(c)?;
        }
        if it.d && it.s != 0 {
            return Err("清单不对");
        }
        total = total.checked_add(it.s).ok_or("清单不对")?;
        let key = it.p.to_lowercase();
        if comps.len() > 1 {
            let parent = key.rsplit_once('\\').map(|(p, _)| p).unwrap_or("");
            if seen.get(parent) != Some(&true) {
                return Err("清单不对");
            }
        }
        if seen.insert(key, it.d).is_some() {
            return Err("名字重复");
        }
    }
    if total != m.bytes {
        return Err("清单不对");
    }
    Ok(())
}

// ---------------------------------------------------------------- 描述符（FILEGROUPDESCRIPTORW）

const FD_ATTRIBUTES: u32 = 0x4;
const FD_WRITESTIME: u32 = 0x20;
const FD_FILESIZE: u32 = 0x40;
const FD_UNICODE: u32 = 0x8000_0000;
/// sizeof(FILEDESCRIPTORW)（packed(1)）。
pub const FD_SIZE: usize = 592;

/// FILEGROUPDESCRIPTORW 的字节：cItems(u32) + 每项一个 FILEDESCRIPTORW。按字节偏移写，不对 packed 字段取引用。
pub fn descriptor(m: &Manifest) -> Vec<u8> {
    let mut v = vec![0u8; 4 + FD_SIZE * m.items.len()];
    v[..4].copy_from_slice(&(m.items.len() as u32).to_le_bytes());
    for (k, it) in m.items.iter().enumerate() {
        let b = &mut v[4 + k * FD_SIZE..4 + (k + 1) * FD_SIZE];
        let flags = FD_ATTRIBUTES | FD_FILESIZE | FD_WRITESTIME | FD_UNICODE;
        b[0..4].copy_from_slice(&flags.to_le_bytes());
        // clsid 16 + sizel 8 + pointl 8 = 32 字节留 0
        let attrs = (it.a & KEEP_ATTRS) | if it.d { ATTR_DIR } else { 0 };
        b[36..40].copy_from_slice(&attrs.to_le_bytes());
        // ftCreationTime 40..48、ftLastAccessTime 48..56 留 0；ftLastWriteTime 56..64
        b[56..64].copy_from_slice(&it.m.to_le_bytes());
        b[64..68].copy_from_slice(&((it.s >> 32) as u32).to_le_bytes());
        b[68..72].copy_from_slice(&(it.s as u32).to_le_bytes());
        for (j, u) in it.p.encode_utf16().take(MAX_PATH_UNITS).enumerate() {
            b[72 + 2 * j..74 + 2 * j].copy_from_slice(&u.to_le_bytes());
        }
    }
    v
}

// ---------------------------------------------------------------- 全局

/// 主会话的情况（net 在会话建立时交来）。
#[derive(Clone, Debug)]
pub struct SessInfo {
    pub sid: u64,
    /// 本机是主会话的客户端：文件连接由本机拨。
    pub client: bool,
    /// 对方地址（客户端 = 拨的地址 = 对方 IP:端口；经线时是本机的桥）。
    pub peer: SocketAddr,
    pub cable: bool,
    pub peer_id: u64,
    pub ticket: [u8; 32],
}

struct Link {
    sess: Option<SessInfo>,
    /// 主会话断开的时间（还没有新会话）。
    sess_lost: Option<Instant>,
    /// 最近一个会话的对方 id（变了 = 对方重启）。
    peer_id: u64,
    conn: Option<Arc<conn::Conn>>,
    /// 有人等着用文件连接（拉清单、读文件、预取）。
    want: bool,
    /// 上次请对方拨（本机是服务端时）。
    asked: Option<Instant>,
    /// 拨号失败次数（退避用）。
    fails: u32,
    /// 最近一次有人用文件连接。
    used: Instant,
}

pub struct Files {
    pub hub: Arc<Hub>,
    pub cable: Option<Arc<Cable>>,
    link: Mutex<Link>,
    link_cv: Condvar,
    next_conn: AtomicU64,
    pub serve: serve::Table,
    pub fetch: fetch::Fetcher,
}

static FILES: OnceLock<Arc<Files>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Files>> {
    FILES.get()
}

/// 用户在灵动岛上点了取消：正在进行的粘贴停下（资源管理器按“用户取消”处理，删半截文件）。
pub fn cancel_paste() -> bool {
    get().is_some_and(|f| f.fetch.cancel_paste())
}

/// 启动（net::spawn 里调用一次）。
pub fn start(hub: Arc<Hub>, cable: Option<Arc<Cable>>) {
    let f = Arc::new(Files {
        hub,
        cable,
        link: Mutex::new(Link { sess: None, sess_lost: None, peer_id: 0, conn: None, want: false, asked: None, fails: 0, used: Instant::now() }),
        link_cv: Condvar::new(),
        next_conn: AtomicU64::new(1),
        serve: serve::Table::default(),
        fetch: fetch::Fetcher::default(),
    });
    if FILES.set(f.clone()).is_err() {
        return;
    }
    serve::start(f.clone());
    ole::start();
    prefetch::start();
    let me = f.clone();
    spawn("files-dial", move || me.run_dialer());
    let me = f.clone();
    spawn("files-tick", move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            for (r, n) in me.fetch.tick() {
                me.hub.to_desktop(crate::hub::ToDesktop::FilesNote(r, n));
            }
        }
    });
}

/// 本机剪贴板有了新内容：之前发出的清单变旧（10 分钟没人要才作废），对方文件的预取作废。
pub fn local_changed() {
    if let Some(f) = get() {
        f.serve.retire();
    }
    prefetch::cancel();
}

/// 本机复制了文件：在工作线程里展开，结果经 ToDesktop::FilesScanned 回 desktop。
pub fn scan_async(g: u64, paths: Vec<std::path::PathBuf>) {
    let Some(f) = get() else { return };
    let hub = f.hub.clone();
    spawn("files-scan", move || {
        let r = scan::expand(&paths);
        hub.to_desktop(crate::hub::ToDesktop::FilesScanned { g, r });
    });
}

/// 对方复制了文件：在工作线程里拉完整清单，结果经 ToDesktop::FilesReady 回 desktop。
pub fn list_async(s: Summary, g: u64, ver: u64) {
    let Some(f) = get() else { return };
    let me = f.clone();
    spawn("files-list", move || {
        let r = me.fetch.get_list(&s);
        if let Err(e) = &r {
            crate::log!("文件：拉对方的清单失败：{e}");
        }
        me.hub.to_desktop(crate::hub::ToDesktop::FilesReady { g, ver, r });
    });
}

/// 主会话建立（net::install）。
pub fn session_up(s: SessInfo) {
    let Some(f) = get() else { return };
    let restarted = {
        let mut l = lock(&f.link);
        let restarted = l.peer_id != 0 && s.peer_id != 0 && l.peer_id != s.peer_id;
        l.peer_id = s.peer_id;
        l.sess = Some(s);
        l.sess_lost = None;
        l.asked = None;
        l.fails = 0;
        restarted
    };
    if restarted {
        crate::log!("文件：对方重启了，作废它之前的清单");
        f.close_conn("对方重启");
        f.fetch.peer_restarted();
        f.serve.peer_restarted();
        ole::peer_restarted();
    }
    f.link_cv.notify_all();
}

/// 主会话结束（net::end_session）。文件连接先不关：换通道时主会话会很快重建。
pub fn session_down(sid: u64) {
    let Some(f) = get() else { return };
    let mut l = lock(&f.link);
    if l.sess.as_ref().is_some_and(|s| s.sid == sid) {
        l.sess = None;
        l.sess_lost = Some(Instant::now());
    }
    drop(l);
    f.link_cv.notify_all();
}

/// 对方请本机拨文件连接（Msg::FileDial）。
pub fn dial_asked(sid: u64) {
    let Some(f) = get() else { return };
    let mut l = lock(&f.link);
    if l.sess.as_ref().is_some_and(|s| s.sid == sid && s.client) {
        l.want = true;
        l.used = Instant::now();
    }
    drop(l);
    f.link_cv.notify_all();
}

/// listener 收到 KMLF：文件连接的服务端（本线程里做握手，认证后另起读写线程）。
pub fn accept(stream: TcpStream, magic: [u8; 4]) {
    let Some(f) = get() else { return };
    let peer = stream.peer_addr().ok();
    let via_cable = peer.is_some_and(|p| f.cable.as_ref().is_some_and(|c| c.owns(p)));
    let sess = lock(&f.link).sess.clone();
    let Some(sess) = sess else { return };
    match conn::Conn::handshake(f.clone(), stream, false, Some(magic), &sess, via_cable) {
        Ok(c) => f.install(c),
        Err(e) => crate::log!("文件：来自 {} 的文件连接没建成：{e}", peer.map(|p| p.to_string()).unwrap_or_default()),
    }
}

impl Files {
    pub fn conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// 当前文件连接（没有就 None，并请拨号线程去建）。
    pub fn conn(&self) -> Option<Arc<conn::Conn>> {
        let mut l = lock(&self.link);
        l.used = Instant::now();
        if let Some(c) = l.conn.as_ref().filter(|c| c.alive()) {
            return Some(c.clone());
        }
        l.conn = None;
        if !l.want {
            l.want = true;
            drop(l);
            self.link_cv.notify_all();
        }
        None
    }

    /// 当前文件连接（只看，不请拨号线程去建）。
    pub fn current_conn(&self) -> Option<Arc<conn::Conn>> {
        lock(&self.link).conn.clone().filter(|c| c.alive())
    }

    /// 文件连接现在走线吗（没有连接时看线通不通）。
    pub fn via_cable(&self) -> bool {
        match lock(&self.link).conn.as_ref().filter(|c| c.alive()) {
            Some(c) => c.cable,
            None => self.cable.as_ref().is_some_and(|c| c.ready()),
        }
    }

    /// 对方的 instance id（没连过 = 0）。
    pub fn peer_id(&self) -> u64 {
        lock(&self.link).peer_id
    }

    fn install(&self, c: Arc<conn::Conn>) {
        let mut old = None;
        // 换上新连接和作废旧连接的在途请求在 fetch 锁内一起做：新连接上发出的请求不会被误作废
        self.fetch.on_conn_change(
            || {
                let mut l = lock(&self.link);
                l.want = false;
                l.fails = 0;
                l.used = Instant::now();
                // 下次断了马上请对方拨（“2 秒内只请一次”只管等的时候别反复请）
                l.asked = None;
                old = l.conn.replace(c.clone());
                true
            },
            true,
        );
        if let Some(o) = old {
            o.close();
            self.serve.conn_lost(o.id);
        }
        crate::log!("文件：文件连接已建立（{}）", if c.cable { "经 USB 线" } else { "局域网" });
        self.link_cv.notify_all();
    }

    /// 文件连接断了（conn 的读写线程发现）。
    pub fn conn_lost(&self, id: u64, why: &str) {
        let mut was = false;
        // 在 fetch 锁内看：是当前连接，或当前没有连接（已被 conn() / 拨号线程当死连接清掉）→ 在途请求都属于死连接，作废；
        // 已经换上了新连接 → 不动（那些请求是新连接上的）
        self.fetch.on_conn_change(
            || {
                let mut l = lock(&self.link);
                match l.conn.as_ref() {
                    Some(c) if c.id == id => {
                        l.conn = None;
                        was = true;
                        true
                    }
                    Some(_) => false,
                    None => true,
                }
            },
            false,
        );
        self.serve.conn_lost(id);
        if was {
            crate::log!("文件：文件连接断开（{why}）");
        }
        self.link_cv.notify_all();
    }

    fn close_conn(&self, why: &str) {
        let mut c = None;
        self.fetch.on_conn_change(
            || {
                c = lock(&self.link).conn.take();
                c.is_some()
            },
            false,
        );
        if let Some(c) = c {
            crate::log!("文件：关闭文件连接（{why}）");
            c.close();
            self.serve.conn_lost(c.id);
        }
    }

    /// 拨号线程：有人要用、还没有连接时，本机是客户端就拨，是服务端就请对方拨。
    fn run_dialer(self: Arc<Self>) {
        loop {
            // 先问忙不忙再拿 link 锁（fetch 持自己的锁时会拿 link 锁：只许 fetch → link 这个顺序）
            let busy = self.fetch.busy() || self.serve.busy(IDLE_CLOSE);
            let (sess, wait) = {
                let mut l = lock(&self.link);
                // 主会话断了太久 / 文件连接闲置太久：关掉
                let orphan = l.sess.is_none() && l.sess_lost.is_some_and(|t| t.elapsed() > ORPHAN_AFTER);
                let idle = l.conn.is_some() && l.used.elapsed() > IDLE_CLOSE && !busy;
                if orphan || idle {
                    drop(l);
                    self.close_conn(if orphan { "主连接已断" } else { "闲置" });
                    l = lock(&self.link);
                }
                if l.conn.as_ref().is_some_and(|c| !c.alive()) {
                    l.conn = None;
                }
                let need = l.want && l.conn.is_none();
                let sess = if need { l.sess.clone() } else { None };
                let backoff = Duration::from_millis((200u64 << l.fails.min(3)).min(1000));
                if sess.is_none() {
                    let _ = self.link_cv.wait_timeout(l, Duration::from_secs(1));
                    continue;
                }
                (sess, backoff)
            };
            let Some(s) = sess else { continue };
            if s.client {
                let cable_ready = self.cable.as_ref().and_then(|c| c.dial_bulk_addr());
                let addr = match cable_ready {
                    Some(a) => Some((a, true)),
                    None if !s.cable => Some((s.peer, false)),
                    // 主会话走线、线又不通了：等主会话在局域网上重建
                    None => None,
                };
                let ok = match addr {
                    Some((a, cable)) => match conn::Conn::dial(self.clone(), a, &s, cable) {
                        Ok(c) => {
                            self.install(c);
                            true
                        }
                        Err(e) => {
                            crate::log!("文件：拨文件连接 {a} 失败：{e}");
                            false
                        }
                    },
                    None => false,
                };
                if !ok {
                    lock(&self.link).fails += 1;
                    std::thread::sleep(wait);
                }
            } else {
                let ask = {
                    let mut l = lock(&self.link);
                    let due = l.asked.is_none_or(|t| t.elapsed() > Duration::from_secs(2));
                    if due {
                        l.asked = Some(Instant::now());
                    }
                    due
                };
                if ask {
                    self.hub.send(crate::proto::Msg::FileDial);
                }
                let l = lock(&self.link);
                let _ = self.link_cv.wait_timeout(l, Duration::from_millis(500));
            }
        }
    }
}

/// 退出前（main）：唤醒所有等着的 Read、停预取、把剪贴板上的虚拟文件换成真文件或清掉。至多等 limit。
pub fn shutdown(limit: Duration) {
    let Some(f) = get() else { return };
    f.fetch.shutdown();
    prefetch::stop();
    ole::shutdown(limit);
    f.close_conn("退出");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(p: &str, s: u64, d: bool) -> Item {
        Item { p: p.into(), s, m: 0, a: 0, d }
    }

    #[test]
    fn components() {
        for bad in ["", ".", "..", "a:b", "a.", "a ", "CON", "con.txt", "NUL.txt", "CON .txt", "COM1", "COM¹", "LPT3.log", "CONIN$", "a\u{202E}b", "a\tb", "a?b"] {
            assert!(component_ok(bad).is_err(), "{bad:?} 应拒绝");
        }
        for good in ["a", "a.txt", "COM10", "CONSOLE", "报告.pdf", ".gitignore", "a b"] {
            assert!(component_ok(good).is_ok(), "{good:?} 应接受");
        }
        assert!(component_ok(&"x".repeat(255)).is_ok());
        assert_eq!(component_ok(&"x".repeat(256)), Err("路径太长"));
    }

    #[test]
    fn manifest_rules() {
        let ok = Manifest::new(vec![it("d", 0, true), it("d\\a.txt", 3, false), it("d\\e", 0, true), it("b.txt", 5, false)]);
        assert_eq!(validate(&ok), Ok(()));
        // 子项在父文件夹之前
        assert!(validate(&Manifest::new(vec![it("d\\a.txt", 3, false), it("d", 0, true)])).is_err());
        // 父是文件
        assert!(validate(&Manifest::new(vec![it("d", 1, false), it("d\\a", 1, false)])).is_err());
        // 大小写重名
        assert_eq!(validate(&Manifest::new(vec![it("A.txt", 1, false), it("a.TXT", 1, false)])), Err("名字重复"));
        // 穿越
        assert!(validate(&Manifest::new(vec![it("d", 0, true), it("d\\..\\..\\x", 1, false)])).is_err());
        assert!(validate(&Manifest::new(vec![it("a/../x", 1, false)])).is_err());
        // 总数对不上
        let mut m = ok.clone();
        m.bytes += 1;
        assert!(validate(&m).is_err());
        let mut m = ok.clone();
        m.count += 1;
        assert!(validate(&m).is_err());
        // 溢出
        assert!(validate(&Manifest { items: vec![it("a", u64::MAX, false), it("b", 2, false)], bytes: 1, count: 2 }).is_err());
        // 路径长度边界
        let mid_dir = format!("d\\{}", "y".repeat(200));
        let long = |n: usize| format!("{mid_dir}\\{}", "x".repeat(n - 203));
        assert_eq!(validate(&Manifest::new(vec![it("d", 0, true), it(&mid_dir, 0, true), it(&long(259), 1, false)])), Ok(()));
        assert_eq!(validate(&Manifest::new(vec![it("d", 0, true), it(&mid_dir, 0, true), it(&long(260), 1, false)])), Err("路径太长"));
        // 太多
        let many: Vec<Item> = (0..=MAX_ITEMS).map(|i| it(&format!("f{i}"), 0, false)).collect();
        assert_eq!(validate(&Manifest::new(many)), Err("文件太多"));
    }

    #[test]
    fn digest_ignores_order_and_case() {
        let a = Manifest::new(vec![it("A.txt", 1, false), it("b", 2, false)]);
        let b = Manifest::new(vec![it("b", 2, false), it("a.TXT", 1, false)]);
        assert_eq!(a.digest(), b.digest());
        let c = Manifest::new(vec![it("b", 2, false), it("a.txt", 9, false)]);
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn descriptor_layout() {
        let m = Manifest::new(vec![Item { p: "文件夹".into(), s: 0, m: 0, a: 0, d: true }, Item { p: "文件夹\\a.txt".into(), s: 0x1_0000_0002, m: 0x0102_0304_0506_0708, a: 0x1 | 0x800, d: false }]);
        let d = descriptor(&m);
        assert_eq!(FD_SIZE, std::mem::size_of::<windows::Win32::UI::Shell::FILEDESCRIPTORW>());
        assert_eq!(d.len(), 4 + 2 * FD_SIZE);
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 2);
        let f = &d[4 + FD_SIZE..];
        assert_eq!(u32::from_le_bytes(f[36..40].try_into().unwrap()), 0x1); // 只留 KEEP_ATTRS
        assert_eq!(u64::from_le_bytes(f[56..64].try_into().unwrap()), 0x0102_0304_0506_0708);
        assert_eq!(u32::from_le_bytes(f[64..68].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(f[68..72].try_into().unwrap()), 2);
        let want: Vec<u16> = "文件夹\\a.txt".encode_utf16().chain([0]).collect();
        let name: Vec<u16> = (0..want.len()).map(|j| u16::from_le_bytes([f[72 + 2 * j], f[73 + 2 * j]])).collect();
        assert_eq!(name, want);
        let dir = &d[4..];
        assert_eq!(u32::from_le_bytes(dir[36..40].try_into().unwrap()), ATTR_DIR);
    }

    #[test]
    fn summary_words() {
        let mut s = Manifest::new(vec![it("a.txt", 1, false)]).summary(&[1; 16]);
        assert_eq!(s.what(), "a.txt");
        assert_eq!(parse_mid(&s.mid), Some([1; 16]));
        s.tops = 3;
        assert_eq!(s.what(), "3 个文件");
        s.dirs = 3;
        assert_eq!(s.what(), "3 个文件夹");
        s.dirs = 1;
        assert_eq!(s.what(), "3 项");
    }
}
