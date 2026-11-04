//! 配置：%APPDATA%\kmlink\config.toml。设置窗口（另一个进程）写它，主程序每秒看一次修改时间并重新加载。

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
        Self { enabled: true, images: true, max_text_kb: 1024, max_image_mb: 32 }
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
        std::fs::create_dir_all(dir())?;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let tmp = dir().join(format!("config.toml.{}.{}.tmp", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&tmp, s)?;
        let r = std::fs::rename(&tmp, path());
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
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

/// 内容没变就不写（net 那边另有每秒至多一次的限频）。
pub fn save_peer_desk(name: &str, desk: &Desk) {
    let s = serde_json::json!({ "name": name, "monitors": desk.monitors }).to_string();
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

pub fn load_peer_desk() -> Option<(String, Desk)> {
    let s = std::fs::read_to_string(peer_desk_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let monitors = serde_json::from_value(v.get("monitors")?.clone()).ok()?;
    Some((name, Desk::new(monitors)))
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
}
