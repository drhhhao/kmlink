//! 检查更新 / 一键更新两台。
//!
//! 来源：GitHub 最新 Release（`REPO`）。Release 由 CI 发布（.github/workflows/release.yml），附件里有
//! `kmlink.exe` 和 `SHA256SUMS.txt`（两行 `<sha256>  <文件名>`）。
//!
//! 检查：后台线程，启动后 FIRST_CHECK 查一次，之后每 CHECK_EVERY 一次；`check_now()` 立刻查（托盘/设置窗口）。
//!   只有比本机新的版本才算“有新版”；查不到（网络不通等）自动检查时不打扰，手动检查时报失败。
//! 更新（`update_both()`）：对方支持（Hello.caps 带 CAP_UPDATE）就先发 Msg::Update{version}，然后本机：
//!   下载 SHA256SUMS.txt 和 kmlink.exe → 校验长度上限、"MZ" 头、SHA-256 → 写 kmlink.new.exe →
//!   当前 exe 改名 kmlink.old.exe（运行中的 exe 可以改名）→ kmlink.new.exe 改名成当前 exe →
//!   用新 exe 启动 `--updated <旧 pid> <旧版本>` → 本进程退出。新进程先等旧进程退出再抢单实例互斥量，
//!   启动后删掉 kmlink.old.exe，提示“已更新到 x.y.z”。
//!   任何一步失败都还原文件，状态写成 Failed(原因)，本进程照常运行。
//! 对方收到 Msg::Update{version}：version 比本机新，就自己去 GitHub 查；最新版正是（或高于）它才更新——
//!   不信任对方给的下载地址，只认 GitHub 上带校验和的 Release。
//!
//! 网络用 WinHTTP（系统自带，按系统代理设置走，不加依赖）。

use crate::hub::Hub;
use crate::log;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const REPO: &str = "drhhhao/kmlink";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 启动后多久做第一次自动检查（别和开机时的网络、连接抢）。
const FIRST_CHECK: Duration = Duration::from_secs(60);
/// 之后多久自动检查一次。
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// 新 exe 最大字节数（正常 ~2 MB）。
const MAX_EXE: usize = 32 * 1024 * 1024;
/// API / 校验和文件最大字节数。
const MAX_SMALL: usize = 1024 * 1024;
/// 新进程最多等旧进程退出这么久。
const WAIT_OLD_MS: u32 = 15_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// 还没查过（或查过但没什么可说的自动检查）
    Idle,
    Checking,
    /// 手动检查：已是最新
    Latest,
    /// 有新版本
    Available(String),
    /// 正在下载 / 安装这个版本（百分比 0..=100）
    Downloading(String, u8),
    /// 已换好文件，正在重启
    Restarting(String),
    /// 失败（一句话原因）
    Failed(String),
    /// 刚从旧版本更新上来（这次启动是更新后的第一次）
    Updated(String),
}

/// 状态写进 status.json 的那几项（设置窗口读）。
pub fn status_fields() -> crate::config::StatusFile {
    let (k, v, p, e) = match state() {
        State::Idle => ("", String::new(), 0, String::new()),
        State::Checking => ("checking", String::new(), 0, String::new()),
        State::Latest => ("latest", String::new(), 0, String::new()),
        State::Available(v) => ("avail", v, 0, String::new()),
        State::Downloading(v, p) => ("down", v, p, String::new()),
        State::Restarting(v) => ("restart", v, 0, String::new()),
        State::Failed(e) => ("fail", String::new(), 0, e),
        State::Updated(v) => ("done", v, 0, String::new()),
    };
    crate::config::StatusFile { upd: k.into(), upd_ver: v, upd_pct: p, upd_err: e, ..Default::default() }
}

/// 失败原因的短说法（灵动岛，≤ 4 字；没有合适的返回空）。
pub fn short_reason(e: &str) -> &'static str {
    if e.starts_with("连不上") {
        "网络不通"
    } else if e.contains("校验") || e.contains("文件不对") {
        "文件不对"
    } else if e.starts_with("GitHub 上最新") {
        "版本没到"
    } else if e.contains("对方") {
        "对方没跟上"
    } else {
        ""
    }
}

/// 一个 Release 里更新要用的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub exe_url: String,
    pub sums_url: String,
}

enum Cmd {
    /// 立刻检查；true = 手动（已是最新/失败也要报）
    Check(bool),
    /// 更新到最新版；true = 先通知对方一起更新
    Update(bool),
    /// 对方让本机更新到这个版本
    Peer(String),
}

struct Shared {
    state: Mutex<State>,
    tx: Sender<Cmd>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// 当前状态（给托盘 / 状态文件）。
pub fn state() -> State {
    SHARED.get().map(|s| s.state.lock().unwrap().clone()).unwrap_or(State::Idle)
}

/// 立刻检查（手动）。
pub fn check_now() {
    send(Cmd::Check(true));
}

/// 一键更新两台：本机更新到最新版，对方支持就让它也更新。
pub fn update_both() {
    send(Cmd::Update(true));
}

/// 对方发来 Msg::Update{version}。
pub fn peer_asks(version: String) {
    send(Cmd::Peer(version));
}

fn send(c: Cmd) {
    if let Some(s) = SHARED.get() {
        let _ = s.tx.send(c);
    }
}

/// 启动更新线程（自动检查 + 处理命令）。updated_from = 这次是从哪个旧版本更新上来的。
pub fn spawn(hub: Arc<Hub>, updated_from: Option<String>) {
    let (tx, rx) = unbounded();
    let st = if updated_from.is_some() { State::Updated(VERSION.into()) } else { State::Idle };
    if SHARED.set(Shared { state: Mutex::new(st), tx }).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || worker(hub, rx));
}

fn set(hub: &Hub, st: State) {
    if let Some(s) = SHARED.get() {
        let mut g = s.state.lock().unwrap();
        if *g == st {
            return;
        }
        *g = st;
    }
    hub.to_desktop(crate::hub::ToDesktop::Update);
}

fn worker(hub: Arc<Hub>, rx: Receiver<Cmd>) {
    // 上次更新留下的旧 exe（旧进程的光标守护可能还占着一会儿：这里再删一次）
    remove_old();
    let mut wait = FIRST_CHECK;
    loop {
        let cmd = match rx.recv_timeout(wait) {
            Ok(c) => c,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Cmd::Check(false),
            Err(_) => return,
        };
        match cmd {
            Cmd::Check(manual) => {
                wait = CHECK_EVERY;
                remove_old();
                check(&hub, manual);
            }
            Cmd::Update(tell_peer) => {
                // 用刚查到的版本；没查过（或查过已久）就先查
                let rel = match latest_release() {
                    Ok(r) => r,
                    Err(e) => {
                        log!("更新：检查失败（{e}）");
                        set(&hub, State::Failed(e));
                        continue;
                    }
                };
                if !newer(&rel.version, VERSION) {
                    set(&hub, State::Latest);
                    continue;
                }
                if tell_peer && peer_can_update(&hub) {
                    log!("更新：请对方一起更新到 {}", rel.version);
                    hub.send(crate::proto::Msg::Update { version: rel.version.clone() });
                }
                install(&hub, &rel);
            }
            Cmd::Peer(v) => {
                if !newer(&v, VERSION) {
                    log!("更新：对方让本机更新到 {v}，本机已是 {VERSION}，不用更新");
                    continue;
                }
                log!("更新：对方让本机更新到 {v}");
                match latest_release() {
                    Ok(rel) if !newer(&v, &rel.version) => install(&hub, &rel),
                    Ok(rel) => {
                        let e = format!("GitHub 上最新是 {}", rel.version);
                        log!("更新：对方要的 {v} 还查不到（{e}）");
                        set(&hub, State::Failed(e));
                    }
                    Err(e) => {
                        log!("更新：检查失败（{e}）");
                        set(&hub, State::Failed(e));
                    }
                }
            }
        }
    }
}

fn peer_can_update(hub: &Hub) -> bool {
    hub.status().connected && hub.status().peer_caps & crate::proto::CAP_UPDATE != 0
}

fn check(hub: &Hub, manual: bool) {
    // 正在更新时不打断状态
    if matches!(state(), State::Downloading(..) | State::Restarting(_)) {
        return;
    }
    if manual {
        set(hub, State::Checking);
    }
    match latest_release() {
        Ok(rel) if newer(&rel.version, VERSION) => {
            log!("更新：有新版本 {}（本机 {VERSION}）", rel.version);
            set(hub, State::Available(rel.version));
        }
        Ok(_) => set(hub, if manual { State::Latest } else { State::Idle }),
        Err(e) => {
            log!("更新：检查失败（{e}）");
            if manual {
                set(hub, State::Failed(e));
            }
        }
    }
}

/// 下载、校验、换文件、重启。成功时不返回（进程退出由 desktop 收尾）；失败写 Failed。
fn install(hub: &Hub, rel: &Release) {
    let v = rel.version.clone();
    set(hub, State::Downloading(v.clone(), 0));
    let r = (|| -> Result<(), String> {
        if fake().is_some() {
            let exe = std::fs::read(&rel.exe_url).map_err(|e| format!("读不了测试文件（{e}）"))?;
            if !exe.starts_with(b"MZ") {
                return Err("下载的文件不对".into());
            }
            set(hub, State::Downloading(v.clone(), 100));
            return swap_and_restart(&exe, &v);
        }
        let sums = http_get(&rel.sums_url, MAX_SMALL, &mut |_, _| {})?;
        let want = sha_for(&String::from_utf8_lossy(&sums), "kmlink.exe").ok_or("校验和文件里没有 kmlink.exe")?;
        let mut last = 0u8;
        let exe = http_get(&rel.exe_url, MAX_EXE, &mut |got, total| {
            if let Some(t) = total.filter(|t| *t > 0) {
                let p = (got * 100 / t).min(100) as u8;
                if p >= last.saturating_add(5) || p == 100 {
                    last = p;
                    set(hub, State::Downloading(v.clone(), p));
                }
            }
        })?;
        verify(&exe, &want)?;
        swap_and_restart(&exe, &v)
    })();
    match r {
        Ok(()) => {
            log!("更新：已换成 {v}，重启");
            set(hub, State::Restarting(v));
            // 和托盘“退出”一样：input（主线程）和 desktop 都要收到退出，进程才会结束，新进程才能接手
            hub.to_input(crate::hub::ToInput::Quit);
            hub.to_desktop(crate::hub::ToDesktop::Quit);
        }
        Err(e) => {
            log!("更新：失败（{e}）");
            set(hub, State::Failed(e));
        }
    }
}

/// 新 exe 是不是像样：长度、PE 头、SHA-256。
fn verify(exe: &[u8], want_hex: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if exe.len() < 64 * 1024 || !exe.starts_with(b"MZ") {
        return Err("下载的文件不对".into());
    }
    let got: String = Sha256::digest(exe).iter().map(|b| format!("{b:02x}")).collect();
    if !got.eq_ignore_ascii_case(want_hex) {
        return Err("校验不对".into());
    }
    Ok(())
}

fn exe_paths() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let cur = std::env::current_exe().map_err(|e| format!("找不到本程序（{e}）"))?;
    let dir = cur.parent().ok_or("找不到本程序所在目录")?.to_path_buf();
    Ok((cur, dir.join("kmlink.new.exe"), dir.join("kmlink.old.exe")))
}

fn remove_old() {
    if let Ok((_, new, old)) = exe_paths() {
        let _ = std::fs::remove_file(old);
        let _ = std::fs::remove_file(new);
    }
}

fn swap_and_restart(exe: &[u8], version: &str) -> Result<(), String> {
    let (cur, new, old) = exe_paths()?;
    std::fs::write(&new, exe).map_err(|e| format!("写不进新文件（{e}）"))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&cur, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(format!("换不了文件（{e}）"));
    }
    if let Err(e) = std::fs::rename(&new, &cur) {
        let _ = std::fs::rename(&old, &cur);
        let _ = std::fs::remove_file(&new);
        return Err(format!("换不了文件（{e}）"));
    }
    match spawn_new(&cur) {
        Ok(()) => {
            let _ = version;
            Ok(())
        }
        Err(e) => {
            // 新的起不来：换回旧的
            let _ = std::fs::rename(&cur, &new);
            let _ = std::fs::rename(&old, &cur);
            let _ = std::fs::remove_file(&new);
            Err(format!("新版本启动不了（{e}）"))
        }
    }
}

fn spawn_new(exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let mut c = std::process::Command::new(exe);
    c.arg("--updated").arg(std::process::id().to_string()).arg(VERSION);
    if crate::log::CONSOLE.load(std::sync::atomic::Ordering::Relaxed) {
        c.arg("--console");
    }
    if let Some(dir) = exe.parent() {
        c.current_dir(dir);
    }
    c.creation_flags(DETACHED_PROCESS).spawn().map(|_| ())
}

/// 新进程启动时：命令行带 `--updated <旧 pid> <旧版本>` 就先等旧进程退出，返回旧版本（之后提示“已更新”）。
pub fn after_update(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--updated")?;
    let pid: u32 = args.get(i + 1)?.parse().ok()?;
    let old = args.get(i + 2).cloned().unwrap_or_default();
    wait_exit(pid, WAIT_OLD_MS);
    remove_old();
    Some(old)
}

fn wait_exit(pid: u32, ms: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, ms);
            let _ = CloseHandle(h);
        }
    }
}

// ───────────────────────── 版本与 Release 解析（纯逻辑） ─────────────────────────

/// "v1.2.3" / "1.2.3" → (1, 2, 3)。只认三段数字（预发布等后缀不认）。
pub fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let mut it = s.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// a 比 b 新。
pub fn newer(a: &str, b: &str) -> bool {
    match (parse_ver(a), parse_ver(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// GitHub releases/latest 的 JSON → Release（要有 kmlink.exe 和 SHA256SUMS.txt 两个附件，地址只认 https://github.com/）。
pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(serde::Deserialize)]
    struct Rel {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    let r: Rel = serde_json::from_slice(json).map_err(|_| "看不懂 GitHub 的回复".to_string())?;
    if r.draft || r.prerelease {
        return Err("最新的是预发布版".into());
    }
    let ver = parse_ver(&r.tag_name).ok_or("版本号看不懂")?;
    let url = |n: &str| {
        r.assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(n))
            .map(|a| a.browser_download_url.clone())
            .filter(|u| u.starts_with(&format!("https://github.com/{REPO}/releases/download/")))
    };
    let exe_url = url("kmlink.exe").ok_or("新版本缺下载文件")?;
    let sums_url = url("SHA256SUMS.txt").ok_or("新版本缺校验文件")?;
    Ok(Release { version: format!("{}.{}.{}", ver.0, ver.1, ver.2), exe_url, sums_url })
}

/// SHA256SUMS.txt 里某个文件的 sha256（64 位十六进制）。
pub fn sha_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let h = it.next()?;
        let n = it.next()?.trim_start_matches('*');
        (n.eq_ignore_ascii_case(file) && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
    })
}

/// 测试用：KMLINK_UPDATE_FAKE=<版本>:<本地 exe 路径> —— 假装 GitHub 上最新是这个版本，下载换成读这个文件（不校验和）。
fn fake() -> Option<(String, String)> {
    let v = std::env::var("KMLINK_UPDATE_FAKE").ok()?;
    let (ver, path) = v.split_once(':')?;
    Some((ver.to_string(), path.to_string()))
}

fn latest_release() -> Result<Release, String> {
    if let Some((ver, path)) = fake() {
        return Ok(Release { version: ver, exe_url: path, sums_url: String::new() });
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = http_get(&url, MAX_SMALL, &mut |_, _| {})?;
    parse_release(&body)
}

// ───────────────────────── WinHTTP ─────────────────────────

/// GET 一个 https 地址（跟随跳转），最多 max 字节。progress(已收, 总长)。
fn http_get(url: &str, max: usize, progress: &mut dyn FnMut(usize, Option<usize>)) -> Result<Vec<u8>, String> {
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::{HSTRING, PCWSTR};

    let rest = url.strip_prefix("https://").ok_or("只支持 https")?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));

    struct H(*mut core::ffi::c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }
    let net = |what: &str| format!("连不上 GitHub（{what}）");

    unsafe {
        let agent = HSTRING::from(format!("kmlink/{VERSION}"));
        let ses = H(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if ses.0.is_null() {
            return Err(net("初始化"));
        }
        let _ = WinHttpSetTimeouts(ses.0, 10_000, 10_000, 15_000, 30_000);
        let h = HSTRING::from(host);
        let con = H(WinHttpConnect(ses.0, PCWSTR(h.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0));
        if con.0.is_null() {
            return Err(net("连接"));
        }
        let p = HSTRING::from(path.as_str());
        let verb = HSTRING::from("GET");
        let req = H(WinHttpOpenRequest(
            con.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(net("请求"));
        }
        let hdr: Vec<u16> = "Accept: application/vnd.github+json, application/octet-stream\r\n".encode_utf16().collect();
        WinHttpSendRequest(req.0, Some(&hdr), None, 0, 0, 0).map_err(|_| net("发送"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|_| net("接收"))?;

        let query_num = |flag: u32| -> Option<usize> {
            let mut v: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                req.0,
                flag | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut v as *mut u32 as *mut _),
                &mut len,
                std::ptr::null_mut(),
            )
            .ok()
            .map(|_| v as usize)
        };
        let code = query_num(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if code == 404 {
            return Err("GitHub 上还没有发布版".into());
        }
        if code != 200 {
            return Err(format!("GitHub 回复 {code}"));
        }
        let total = query_num(WINHTTP_QUERY_CONTENT_LENGTH);
        if total.is_some_and(|t| t > max) {
            return Err("文件太大".into());
        }
        let mut body = Vec::with_capacity(total.unwrap_or(0));
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut n: u32 = 0;
            WinHttpReadData(req.0, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut n).map_err(|_| net("下载中断"))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n as usize]);
            if body.len() > max {
                return Err("文件太大".into());
            }
            progress(body.len(), total);
        }
        if total.is_some_and(|t| t != body.len()) {
            return Err(net("下载不完整"));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_ver("v0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse_ver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("1.2"), None);
        assert_eq!(parse_ver("1.2.3-rc1"), None);
        assert_eq!(parse_ver("1.2.3.4"), None);
        assert!(newer("v0.1.10", "0.1.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.1"));
        assert!(!newer("garbage", "0.1.0"));
    }

    fn rel_json(tag: &str, assets: &[(&str, &str)], pre: bool) -> Vec<u8> {
        let a: Vec<String> = assets.iter().map(|(n, u)| format!(r#"{{"name":"{n}","browser_download_url":"{u}","size":1}}"#)).collect();
        format!(r#"{{"tag_name":"{tag}","draft":false,"prerelease":{pre},"assets":[{}],"body":"x"}}"#, a.join(",")).into_bytes()
    }

    #[test]
    fn release_parsing() {
        let base = format!("https://github.com/{REPO}/releases/download/v0.1.1/");
        let exe = format!("{base}kmlink.exe");
        let sums = format!("{base}SHA256SUMS.txt");
        let zip = format!("{base}kmlink-v0.1.1-windows-x64.zip");
        let r = parse_release(&rel_json(
            "v0.1.1",
            &[("kmlink-v0.1.1-windows-x64.zip", &zip), ("kmlink.exe", &exe), ("SHA256SUMS.txt", &sums)],
            false,
        ))
        .unwrap();
        assert_eq!(r, Release { version: "0.1.1".into(), exe_url: exe.clone(), sums_url: sums.clone() });
        // 缺 exe（比如 v0.1.0 只有 zip）
        assert!(parse_release(&rel_json("v0.1.0", &[("kmlink-v0.1.0-windows-x64.zip", &zip), ("SHA256SUMS.txt", &sums)], false)).is_err());
        // 预发布不算
        assert!(parse_release(&rel_json("v0.1.1", &[("kmlink.exe", &exe), ("SHA256SUMS.txt", &sums)], true)).is_err());
        // 别的地方的下载地址不认
        assert!(
            parse_release(&rel_json("v0.1.1", &[("kmlink.exe", "https://evil.example/kmlink.exe"), ("SHA256SUMS.txt", &sums)], false))
                .is_err()
        );
        assert!(parse_release(b"not json").is_err());
    }

    #[test]
    fn sums_lookup() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let s = format!("{h1}  kmlink-v0.1.1-windows-x64.zip\r\n{h2}  kmlink.exe\r\n");
        assert_eq!(sha_for(&s, "kmlink.exe"), Some("b".repeat(64)));
        assert_eq!(sha_for(&s, "KMLINK.EXE"), Some("b".repeat(64)));
        assert_eq!(sha_for(&format!("{h1} *kmlink.exe"), "kmlink.exe"), Some(h1.clone()));
        assert_eq!(sha_for("abc  kmlink.exe", "kmlink.exe"), None);
        assert_eq!(sha_for(&s, "other.exe"), None);
    }

    /// 真连 GitHub（手动跑：cargo test -- --ignored live_github）。
    #[test]
    #[ignore]
    fn live_github() {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let body = http_get(&url, MAX_SMALL, &mut |_, _| {}).expect("GET");
        let r = parse_release(&body);
        println!("latest: {r:?}");
        let tag = String::from_utf8_lossy(&body);
        assert!(tag.contains("tag_name"));
    }

    #[test]
    fn verify_checks_header_and_hash() {
        use sha2::{Digest, Sha256};
        let mut exe = vec![0u8; 70 * 1024];
        exe[0] = b'M';
        exe[1] = b'Z';
        let h: String = Sha256::digest(&exe).iter().map(|b| format!("{b:02x}")).collect();
        assert!(verify(&exe, &h).is_ok());
        assert!(verify(&exe, &"0".repeat(64)).is_err());
        let mut bad = exe.clone();
        bad[0] = b'X';
        assert!(verify(&bad, &h).is_err());
        assert!(verify(&exe[..1000], &h).is_err());
    }
}
