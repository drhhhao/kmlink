//! 配置：%APPDATA%\kmlink\config.toml。设置窗口（另一个进程）写它，主程序每秒看一次修改时间并重新加载。
//!
//! 同目录下另有几个主程序和设置窗口之间传话的小文件（JSON）：
//!   peer.json        主程序写：对方名字、显示器、每块的有效 DPI（dpis，和 monitors 一一对应；旧对方没有 = 不知道）。
//!   status.json      主程序写（启动时、连接状态变化时、退出时）：pid、是否连着、对方名字/地址、是否暂停、时间。
//!                    原子写（临时文件再改名）。设置窗口只在 connected 且 pid 还活着时认为连着。
//!   rename_peer.json 设置窗口写（给对方改名）：{name, t}。主程序每秒看一次，读了就删；连着且 t 在 10 秒内才发给对方。
//! 名字（本机、对方）：去掉首尾空白，不能为空，最多 NAME_MAX 个字符。

use crate::geom::{Desk, Layout};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 47900;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 本机名字（显示用），默认计算机名。
    pub name: String,
    /// 本机监听端口（TCP 和 UDP 同号）。
    pub port: u16,
    /// 对方地址（IP 或 IP:端口）。留空则只等对方连进来。两台里至少一台要填。
    pub peer: String,
    /// 配对密钥：64 个十六进制字符（32 字节），两台必须一样。
    pub key: String,
    pub layout: LayoutCfg,
    pub input: InputCfg,
    pub clipboard: ClipCfg,
    /// 开机自启（写 HKCU\...\Run）。
    pub autostart: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutCfg {
    #[serde(flatten)]
    pub layout: Layout,
    /// 修改时间（毫秒）。两台同步排布时，较新的覆盖较旧的。
    pub rev: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InputCfg {
    /// 在对方屏幕上的移动速度倍数。
    pub speed: f64,
    /// 强制回到本机 / 在本机时切换“锁定不跨屏”的热键（虚拟键码，默认 Scroll Lock = 0x91）。
    pub hotkey_vk: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipCfg {
    pub enabled: bool,
    pub images: bool,
    pub max_text_kb: u32,
    pub max_image_mb: u32,
    /// 复制后在屏幕底部弹提示（两台同时弹）。
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into()),
            port: DEFAULT_PORT,
            peer: String::new(),
            key: String::new(),
            layout: LayoutCfg::default(),
            input: InputCfg::default(),
            clipboard: ClipCfg::default(),
            autostart: false,
        }
    }
}

impl Default for LayoutCfg {
    fn default() -> Self {
        Self { layout: Layout::default(), rev: 0 }
    }
}

impl Default for InputCfg {
    fn default() -> Self {
        Self { speed: 1.0, hotkey_vk: 0x91 }
    }
}

impl Default for ClipCfg {
    fn default() -> Self {
        Self { enabled: true, images: true, max_text_kb: 1024, max_image_mb: 32, notify: true }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("kmlink")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// 设置窗口用：对方桌面的显示器（主程序在收到对方 Hello 时写入）。
pub fn peer_desk_path() -> PathBuf {
    dir().join("peer.json")
}

/// 主程序的连接状态（设置窗口读）。
pub fn status_path() -> PathBuf {
    dir().join("status.json")
}

/// 设置窗口要给对方改名（主程序读了就删）。
pub fn rename_peer_path() -> PathBuf {
    dir().join("rename_peer.json")
}

impl Config {
    /// 读取配置；不存在就创建一个带新密钥的默认配置。返回 (配置, 是否新建)。
    /// 文件存在但解析失败：备份为 config.bad-<时间>.toml，尽量保留其中的 key/peer，绝不悄悄换掉密钥。
    pub fn load_or_create() -> (Config, bool) {
        let _g = FileLock::acquire();
        let p = path();
        match std::fs::read_to_string(&p) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut c) => {
                    if c.key_bytes().is_none() {
                        crate::log!("config.toml 里的 key 无效，已生成新密钥（两台需重新配对）");
                        c.key = new_key_hex();
                        let _ = c.save_locked();
                    }
                    (c, false)
                }
                Err(e) => {
                    let name = format!("config.bad-{}.toml", now_ms());
                    let bak = dir().join(&name);
                    let _ = std::fs::copy(&p, &bak);
                    prune_bad(&name);
                    crate::log!("config.toml 解析失败（{e}），已备份到 {}", bak.display());
                    let mut c = Config::default();
                    c.key = salvage(&s, "key").filter(|k| parse_key(k).is_some()).unwrap_or_else(new_key_hex);
                    if let Some(peer) = salvage(&s, "peer") {
                        c.peer = peer;
                    }
                    let _ = c.save_locked();
                    (c, false)
                }
            },
            Err(_) => {
                let c = Config { key: new_key_hex(), ..Default::default() };
                let _ = c.save_locked();
                (c, true)
            }
        }
    }

    /// 只读不写；解析失败返回 None。
    pub fn load() -> Option<Config> {
        let s = std::fs::read_to_string(path()).ok()?;
        toml::from_str(&s).ok()
    }

    /// 整个保存（拿跨进程锁）。多数情况应该用 `update`，只改自己关心的字段。
    #[allow(dead_code)]
    pub fn save(&self) -> std::io::Result<()> {
        let _g = FileLock::acquire();
        self.save_locked()
    }

    /// 内容没变就不写（避免无谓的磁盘写入和设置窗口/主程序互相触发重新加载）。
    fn save_locked(&self) -> std::io::Result<()> {
        let s = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        if std::fs::read_to_string(path()).is_ok_and(|old| old == s) {
            return Ok(());
        }
        write_atomic(&path(), &s)
    }

    pub fn key_bytes(&self) -> Option<[u8; 32]> {
        parse_key(&self.key)
    }

    /// 对方的 socket 地址（未填端口时用本机端口）。
    #[allow(dead_code)]
    pub fn peer_addr(&self) -> Option<String> {
        let p = self.peer.trim();
        if p.is_empty() {
            return None;
        }
        Some(if p.contains(':') { p.to_string() } else { format!("{p}:{}", self.port) })
    }
}

/// 读-改-写配置（跨进程加锁）：从磁盘读最新的（读不到就用 `fallback`），交给 f 改，再保存。
/// 主程序、设置窗口都用它，只改自己负责的字段，避免互相覆盖。
pub fn update(fallback: &Config, f: impl FnOnce(&mut Config)) -> std::io::Result<Config> {
    let _g = FileLock::acquire();
    let mut c = Config::load().unwrap_or_else(|| fallback.clone());
    f(&mut c);
    c.save_locked()?;
    Ok(c)
}

/// 原子写：先写同目录的临时文件（<文件名>.<pid>.<序号>.tmp）再改名，读的一方不会读到写了一半的。
fn write_atomic(p: &std::path::Path, s: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("kmlink");
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = p.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
    std::fs::write(&tmp, s)?;
    let r = std::fs::rename(&tmp, p);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// 排布版本号：只在排布真的变了时调用。取 max(当前时间, 旧值 + 1)，两台时钟不准也不会倒退（Lamport）。
pub fn next_rev(old: u64) -> u64 {
    now_ms().max(old.saturating_add(1))
}

/// 跨进程的配置文件锁（命名互斥量 Local\\kmlink-config）。
struct FileLock(windows::Win32::Foundation::HANDLE);

impl FileLock {
    fn acquire() -> FileLock {
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        unsafe {
            match CreateMutexW(None, false, windows::core::w!("Local\\kmlink-config")) {
                Ok(h) => {
                    let _ = WaitForSingleObject(h, 3000);
                    FileLock(h)
                }
                Err(_) => FileLock(windows::Win32::Foundation::HANDLE::default()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;
        if !self.0.is_invalid() {
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 从解析不了的 toml 里尽量捞出 `name = "value"`。
fn salvage(s: &str, name: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix(name)?.trim_start().strip_prefix('=')?.trim();
        let v = rest.strip_prefix('"')?;
        Some(v[..v.find('"')?].to_string())
    })
}

pub fn parse_key(s: &str) -> Option<[u8; 32]> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn new_key_hex() -> String {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).expect("getrandom");
    k.iter().map(|b| format!("{b:02x}")).collect()
}

/// 内容没变就不写（net 那边另有每秒至多一次的限频）。dpis 和 desk.monitors 一一对应（可以更短 = 后面的不知道）。
pub fn save_peer_desk(name: &str, desk: &Desk, dpis: &[u32]) {
    let s = peer_desk_json(name, desk, dpis);
    let p = peer_desk_path();
    if std::fs::read_to_string(&p).is_ok_and(|old| old == s) {
        return;
    }
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(p, s);
}

/// 解析失败的备份里有密钥：只留最新的一个（keep），其余 config.bad-*.toml 删掉。
fn prune_bad(keep: &str) {
    let Ok(rd) = std::fs::read_dir(dir()) else { return };
    for e in rd.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if is_bad_backup(n) && n != keep {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn is_bad_backup(n: &str) -> bool {
    n.starts_with("config.bad-") && n.ends_with(".toml")
}

#[allow(dead_code)] // 设置窗口用
pub fn load_peer_desk() -> Option<(String, Desk)> {
    load_peer_desk_dpis().map(|(name, desk, _)| (name, desk))
}

/// 对方名字、显示器、每块的有效 DPI（96 = 100%）。dpis 可能比显示器少（旧版本对方）：缺的当不知道。
#[allow(dead_code)] // 设置窗口用
pub fn load_peer_desk_dpis() -> Option<(String, Desk, Vec<u32>)> {
    parse_peer_desk(&std::fs::read_to_string(peer_desk_path()).ok()?)
}

fn peer_desk_json(name: &str, desk: &Desk, dpis: &[u32]) -> String {
    serde_json::json!({ "name": name, "monitors": desk.monitors, "dpis": dpis }).to_string()
}

fn parse_peer_desk(s: &str) -> Option<(String, Desk, Vec<u32>)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let monitors: Vec<crate::geom::Rect> = serde_json::from_value(v.get("monitors")?.clone()).ok()?;
    let mut dpis: Vec<u32> = v.get("dpis").and_then(|d| serde_json::from_value(d.clone()).ok()).unwrap_or_default();
    dpis.truncate(monitors.len());
    Some((name, Desk::new(monitors), dpis))
}

// ---------- 名字、status.json、rename_peer.json ----------

/// 名字最多这么多个字符。
pub const NAME_MAX: usize = 16;
/// rename_peer.json 的有效期（毫秒）：更旧的（比如当时主程序没在运行）不发。
pub const RENAME_TTL_MS: u64 = 10_000;

/// 清理名字：去掉首尾空白，截到 NAME_MAX 个字符；空的返回 None。
pub fn clean_name(s: &str) -> Option<String> {
    let s: String = s.trim().chars().take(NAME_MAX).collect();
    let s = s.trim_end();
    (!s.is_empty()).then(|| s.to_string())
}

/// status.json 的内容。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusFile {
    pub pid: u32,
    pub connected: bool,
    pub peer_name: String,
    pub peer_addr: String,
    pub paused: bool,
    /// 写入时间（毫秒）。
    pub t: u64,
}

/// 原子写 status.json。
pub fn save_status(st: &StatusFile) -> std::io::Result<()> {
    let s = serde_json::to_string(st).map_err(std::io::Error::other)?;
    write_atomic(&status_path(), &s)
}

#[allow(dead_code)] // 设置窗口用
pub fn load_status() -> Option<StatusFile> {
    serde_json::from_str(&std::fs::read_to_string(status_path()).ok()?).ok()
}

#[derive(Serialize, Deserialize)]
struct RenamePeer {
    name: String,
    t: u64,
}

/// rename_peer.json 的内容（设置窗口写）。
#[allow(dead_code)] // 设置窗口用
pub fn rename_peer_json(name: &str, t: u64) -> String {
    serde_json::to_string(&RenamePeer { name: name.to_string(), t }).unwrap_or_default()
}

/// 解析 rename_peer.json：名字清理后有效、t 和 now 相差不超过 RENAME_TTL_MS 才返回。
pub fn parse_rename_peer(s: &str, now: u64) -> Option<String> {
    let r: RenamePeer = serde_json::from_str(s).ok()?;
    if now.abs_diff(r.t) > RENAME_TTL_MS {
        return None;
    }
    clean_name(&r.name)
}

/// 主程序每秒调一次：有 rename_peer.json 就读出来并删掉。没有文件返回 None；有但无效/过期返回 Some(None)。
/// 解析不了且是 2 秒内写的：可能还没写完，留到下一秒再看。
pub fn take_rename_peer(now: u64) -> Option<Option<String>> {
    let p = rename_peer_path();
    let s = std::fs::read_to_string(&p).ok()?;
    if serde_json::from_str::<RenamePeer>(&s).is_err() {
        let fresh = std::fs::metadata(&p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() < 2));
        if fresh {
            return None;
        }
    }
    let _ = std::fs::remove_file(&p);
    Some(parse_rename_peer(&s, now))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_toml() {
        let c = Config { key: new_key_hex(), peer: "10.0.0.2".into(), ..Default::default() };
        let s = toml::to_string_pretty(&c).unwrap();
        let d: Config = toml::from_str(&s).unwrap();
        assert_eq!(d.key, c.key);
        assert_eq!(d.layout.layout, c.layout.layout);
        assert_eq!(d.peer_addr().unwrap(), "10.0.0.2:47900");
        assert!(parse_key(&d.key).is_some());
        // 缺字段时用默认值
        let e: Config = toml::from_str("peer = \"1.2.3.4:5\"").unwrap();
        assert_eq!(e.port, DEFAULT_PORT);
        assert_eq!(e.peer_addr().unwrap(), "1.2.3.4:5");
    }

    #[test]
    fn parse_key_rejects_non_ascii() {
        let mut k = "a".repeat(62);
        k.push('中'); // 3 字节，总长 65 —— 不能 panic
        assert!(parse_key(&k).is_none());
        assert!(parse_key(&"g".repeat(64)).is_none());
        assert!(parse_key(&"Ab".repeat(32)).is_some());
    }

    #[test]
    fn salvage_and_rev() {
        let broken = "key = \"abcd\"
peer = \"10.0.0.1\"
[layout
";
        assert_eq!(salvage(broken, "key").as_deref(), Some("abcd"));
        assert_eq!(salvage(broken, "peer").as_deref(), Some("10.0.0.1"));
        assert_eq!(salvage(broken, "port"), None);
        let far_future = now_ms() + 10_000_000;
        assert_eq!(next_rev(far_future), far_future + 1);
        assert!(next_rev(0) >= now_ms() - 1000);
        assert!(is_bad_backup("config.bad-123.toml"));
        assert!(!is_bad_backup("config.toml"));
        assert!(!is_bad_backup("config.bad-1.toml.tmp"));
    }

    #[test]
    fn peer_desk_json_dpis() {
        use crate::geom::Rect;
        let d = Desk::new(vec![Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 4480, 1440)]);
        let s = peer_desk_json("书房", &d, &[96, 144]);
        assert_eq!(parse_peer_desk(&s), Some(("书房".into(), d.clone(), vec![96, 144])));
        // 旧格式（没有 dpis）
        let old = serde_json::json!({ "name": "A", "monitors": d.monitors }).to_string();
        assert_eq!(parse_peer_desk(&old), Some(("A".into(), d.clone(), vec![])));
        // dpis 不完整 / 多余 / 类型不对
        assert_eq!(parse_peer_desk(&peer_desk_json("A", &d, &[120])).unwrap().2, vec![120]);
        assert_eq!(parse_peer_desk(&peer_desk_json("A", &d, &[96, 96, 96])).unwrap().2, vec![96, 96]);
        let bad = serde_json::json!({ "name": "A", "monitors": d.monitors, "dpis": "x" }).to_string();
        assert_eq!(parse_peer_desk(&bad).unwrap().2, Vec::<u32>::new());
        assert!(parse_peer_desk("{}").is_none());
    }

    #[test]
    fn names_status_rename() {
        assert_eq!(clean_name("  书房电脑 ").as_deref(), Some("书房电脑"));
        assert_eq!(clean_name(" \t "), None);
        assert_eq!(clean_name(&"中".repeat(20)).unwrap().chars().count(), NAME_MAX);
        // 截断后末尾的空白也去掉
        assert_eq!(clean_name(&format!("{} x", "a".repeat(15))), Some("a".repeat(15)));

        let st = StatusFile { pid: 42, connected: true, peer_name: "客厅".into(), peer_addr: "10.0.0.2:47900".into(), paused: false, t: 7 };
        let s = serde_json::to_string(&st).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        for k in ["pid", "connected", "peer_name", "peer_addr", "paused", "t"] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(serde_json::from_str::<StatusFile>(&s).unwrap(), st);

        let now = 1_000_000;
        assert_eq!(parse_rename_peer(&rename_peer_json(" 客厅 ", now - 500), now).as_deref(), Some("客厅"));
        assert_eq!(parse_rename_peer(&rename_peer_json("客厅", now - RENAME_TTL_MS - 1), now), None); // 过期
        assert_eq!(parse_rename_peer(&rename_peer_json("客厅", now + RENAME_TTL_MS + 1), now), None); // 时间离谱
        assert_eq!(parse_rename_peer(&rename_peer_json("  ", now), now), None);
        assert_eq!(parse_rename_peer(r#"{"name":"a"}"#, now), None);
        assert_eq!(parse_rename_peer("garbage", now), None);
    }
}
