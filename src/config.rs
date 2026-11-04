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
    pub fn load_or_create() -> (Config, bool) {
        let p = path();
        if let Ok(s) = std::fs::read_to_string(&p) {
            match toml::from_str::<Config>(&s) {
                Ok(mut c) => {
                    if c.key_bytes().is_none() {
                        c.key = new_key_hex();
                        let _ = c.save();
                    }
                    return (c, false);
                }
                Err(e) => crate::log!("config.toml 解析失败（{e}），使用默认配置"),
            }
        }
        let c = Config { key: new_key_hex(), ..Default::default() };
        let _ = c.save();
        (c, true)
    }

    pub fn load() -> Option<Config> {
        let s = std::fs::read_to_string(path()).ok()?;
        toml::from_str(&s).ok()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(dir())?;
        let s = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path().with_extension("toml.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(tmp, path())
    }

    pub fn key_bytes(&self) -> Option<[u8; 32]> {
        parse_key(&self.key)
    }

    /// 对方的 socket 地址（未填端口时用本机端口）。
    pub fn peer_addr(&self) -> Option<String> {
        let p = self.peer.trim();
        if p.is_empty() {
            return None;
        }
        Some(if p.contains(':') { p.to_string() } else { format!("{p}:{}", self.port) })
    }
}

pub fn parse_key(s: &str) -> Option<[u8; 32]> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() != 64 {
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

pub fn save_peer_desk(name: &str, desk: &Desk) {
    let v = serde_json::json!({ "name": name, "monitors": desk.monitors });
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(peer_desk_path(), v.to_string());
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
}
