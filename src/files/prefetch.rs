//! B 端：微信 / QQ 兜底。插着线、总共 ≤ 50 MB 时，写入剪贴板后就把文件先取到本机临时目录，
//! 全部写完、核对过长度，才让剪贴板换成带 CF_HDROP（真文件路径）的对象 —— 绝不交出半截文件。
//!
//! - 只在走线时预取（局域网上不预取，用户定的）；中途线断了 → 放弃，删掉这次的目录。
//! - 资源管理器正在读这份清单时暂停；来了新清单立即放弃旧的。
//! - 目录：<根>\<随机名>\，各项按相对路径建；每个文件先写 .part，写满声明的大小再改名、设修改时间。
//!   根：以管理员身份运行时是 %ProgramData%\kmlink-files（属主 Administrators、受保护的 ACL：普通用户只能读），
//!   否则是 %TEMP%\kmlink-files。管理员进程绝不在普通权限程序能改的目录里写 / 删（防联接点把写删引到别处）。
//! - 剩余空间不到 总量 + max(512 MB, 5%) 就不预取。
//! - 清理：每 5 分钟看一次，不是当前剪贴板用着的、30 分钟没动过的目录删掉；启动时删 24 小时以前的。
//!   本进程启动前就有的目录（上次退出时剪贴板可能还指着它）一律按 24 小时算。

use super::fetch::Remote;
use crossbeam_channel::{Sender, unbounded};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const BUF: usize = 256 << 10;
const KEEP: Duration = Duration::from_secs(30 * 60);
const KEEP_STARTUP: Duration = Duration::from_secs(24 * 3600);
const CLEAN_EVERY: Duration = Duration::from_secs(300);
const MIN_FREE: u64 = 512 << 20;

static TX: OnceLock<Sender<(Arc<Remote>, u64)>> = OnceLock::new();
static GEN: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
/// 剪贴板当前（可能）用着的临时目录：清理时不删。
static IN_USE: Mutex<Option<PathBuf>> = Mutex::new(None);
static STARTED: OnceLock<SystemTime> = OnceLock::new();

/// 管理员身份运行时的根：ProgramData 取系统登记的位置（不看环境变量：普通权限的程序能改 HKCU\Environment）。
fn root_path() -> Option<PathBuf> {
    if !crate::elev::self_elevated() {
        return Some(std::env::temp_dir().join("kmlink-files"));
    }
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY：返回的字符串用完 CoTaskMemFree
    unsafe {
        let w = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None).ok()?;
        let s = w.to_string().ok();
        CoTaskMemFree(Some(w.0 as *const _));
        s.map(|s| PathBuf::from(s).join("kmlink-files"))
    }
}

/// 建好并核对根目录。None = 不安全或建不了（不预取、不清理）。
fn root() -> Option<PathBuf> {
    let p = root_path()?;
    if crate::elev::self_elevated() {
        if let Err(e) = secure(&p) {
            crate::log!("文件：预取目录 {} 不安全或设不了权限（{e}），不预取", p.display());
            return None;
        }
    } else {
        let _ = std::fs::create_dir(&p);
        if std::fs::symlink_metadata(&p).map_or(true, |m| !m.is_dir()) {
            return None;
        }
    }
    Some(p)
}

/// 受保护的权限：属主 Administrators；SYSTEM、Administrators 完全控制，Users 只读（微信、资源管理器要读）。
const ROOT_SDDL: windows::core::PCWSTR = windows::core::w!("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)");

/// 管理员身份运行时：根目录要么由本程序带着受保护的权限一次建好（中间没有空档），要么已存在且属主是
/// Administrators / SYSTEM（只有管理员能把属主设成它们：普通权限的程序抢先建的不会是）。再以不跟随联接点的方式
/// 打开核对是真目录、重设一遍权限。之后普通权限的程序改不了、删不了、改不了名（ProgramData 不给普通用户删子项），
/// 里面也建不了联接点。
fn secure(p: &Path) -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo};
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    };
    let w = crate::sys::wide(&p.to_string_lossy());
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY：sd 由系统分配，函数末尾 LocalFree；dacl 指向 sd 内部，只在 sd 释放前用；句柄在下面关掉
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(ROOT_SDDL, SDDL_REVISION_1, &mut sd, None).map_err(|e| format!("生成权限失败：{e}"))?;
        let r = (|| {
            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
            let created = match CreateDirectoryW(windows::core::PCWSTR(w.as_ptr()), Some(&sa)) {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => false,
                Err(e) => return Err(format!("建不了：{e}")),
            };
            let access = (READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).0;
            let h = CreateFileW(
                windows::core::PCWSTR(w.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(|e| format!("打不开：{e}"))?;
            let r = (|| {
                let mut info = BY_HANDLE_FILE_INFORMATION::default();
                GetFileInformationByHandle(h, &mut info).map_err(|e| format!("读属性失败：{e}"))?;
                if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                    return Err("不是普通目录（联接点 / 链接）".to_string());
                }
                if !created {
                    let mut owner = PSID::default();
                    let mut osd = PSECURITY_DESCRIPTOR::default();
                    let e = GetSecurityInfo(h, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, Some(&mut owner), None, None, None, Some(&mut osd));
                    if e.is_err() {
                        return Err(format!("读属主失败：{e:?}"));
                    }
                    let ok = IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool() || IsWellKnownSid(owner, WinLocalSystemSid).as_bool();
                    let _ = LocalFree(Some(HLOCAL(osd.0)));
                    if !ok {
                        return Err("属主不是管理员（可能是别的程序抢先建的）".to_string());
                    }
                }
                let mut dacl: *mut ACL = std::ptr::null_mut();
                let (mut d1, mut d2) = (Default::default(), Default::default());
                GetSecurityDescriptorDacl(sd, &mut d1, &mut dacl, &mut d2).map_err(|e| format!("{e}"))?;
                let e = SetSecurityInfo(h, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, None, None, Some(dacl), None);
                if e.is_err() {
                    return Err(format!("设权限失败：{e:?}"));
                }
                Ok(())
            })();
            let _ = CloseHandle(h);
            r
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        r
    }
}

pub fn start() {
    let (tx, rx) = unbounded::<(Arc<Remote>, u64)>();
    let _ = TX.set(tx);
    let _ = STARTED.set(SystemTime::now());
    super::spawn("files-prefetch", move || {
        clean(KEEP_STARTUP);
        loop {
            match rx.recv_timeout(CLEAN_EVERY) {
                Ok(mut job) => {
                    // 只做最新的一份（代次在 begin 时就定了：排队期间被取消的，live() 一上来就是 false）
                    while let Ok(n) = rx.try_recv() {
                        job = n;
                    }
                    run(&job.0, job.1);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => clean(KEEP),
                Err(_) => return,
            }
        }
    });
}

/// 写入剪贴板成功后调用：够条件就开始预取（之前没做完的那份作废）。
pub fn begin(r: Arc<Remote>) {
    let g = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(f) = super::get() else { return };
    if r.man.bytes > super::PREFETCH_MAX || !f.via_cable() || r.man.files() == 0 {
        return;
    }
    if let Some(tx) = TX.get() {
        let _ = tx.send((r, g));
    }
}

/// 放弃正在做的预取（本机复制了别的、对方内容换了）。
pub fn cancel() {
    GEN.fetch_add(1, Ordering::SeqCst);
}

pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
    GEN.fetch_add(1, Ordering::SeqCst);
}

/// DROPFILES（fWide = 1）+ 每个路径 UTF-16 带 0 + 结尾再一个 0。
pub fn dropfiles(paths: &[PathBuf]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    v.extend_from_slice(&[0u8; 8]); // pt
    v.extend_from_slice(&0u32.to_le_bytes()); // fNC
    v.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in p.to_string_lossy().encode_utf16().chain([0]) {
            v.extend_from_slice(&u.to_le_bytes());
        }
    }
    v.extend_from_slice(&[0, 0]);
    v
}

fn free_space(p: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w = crate::sys::wide(&p.to_string_lossy());
    let mut free = 0u64;
    // SAFETY：w 以 0 结尾
    unsafe { GetDiskFreeSpaceExW(windows::core::PCWSTR(w.as_ptr()), Some(&mut free), None, None) }.ok()?;
    Some(free)
}

fn filetime_to_system(ft: u64) -> SystemTime {
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    if ft <= EPOCH_DIFF {
        return SystemTime::UNIX_EPOCH;
    }
    SystemTime::UNIX_EPOCH + Duration::from_nanos((ft - EPOCH_DIFF).saturating_mul(100))
}

fn run(r: &Arc<Remote>, g: u64) {
    let Some(files) = super::get() else { return };
    let live = || !STOP.load(Ordering::SeqCst) && GEN.load(Ordering::SeqCst) == g && files.fetch.alive(&r.mid) && files.via_cable();
    let Some(base) = root() else { return };
    let need = r.man.bytes + MIN_FREE.max(r.man.bytes / 20);
    if free_space(&base).is_some_and(|f| f < need) {
        crate::log!("文件：临时目录所在的盘空间不够，不预取");
        return;
    }
    let dir = base.join(super::hex(&super::new_mid()));
    if std::fs::create_dir(&dir).is_err() {
        return;
    }
    let t0 = std::time::Instant::now();
    let ok = fill(r, &dir, &live);
    if !ok {
        // 已取到本机的那几个文件：之后的流改回走网络（目录马上删）
        files.fetch.clear_local(&r.mid);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let tops: Vec<PathBuf> = r.man.tops().map(|i| dir.join(&i.p)).collect();
    *IN_USE.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());
    crate::log!("文件：已预取 {} KB 到临时目录（{:.1} 秒）", r.man.bytes / 1024, t0.elapsed().as_secs_f32());
    super::ole::add_hdrop(r.mid, dropfiles(&tops));
}

fn fill(r: &Arc<Remote>, dir: &Path, live: &dyn Fn() -> bool) -> bool {
    let Some(files) = super::get() else { return false };
    let mut buf = vec![0u8; BUF];
    for i in 0..r.man.items.len() {
        if !fill_one(files, r, i, dir, live, &mut buf) {
            // 半路放弃：丢掉这个文件的缓冲和在途请求（不然一直占着额度）
            files.fetch.drop_file(&r.mid, i as u32);
            return false;
        }
    }
    true
}

fn fill_one(files: &super::Files, r: &Arc<Remote>, i: usize, dir: &Path, live: &dyn Fn() -> bool, buf: &mut [u8]) -> bool {
    use std::io::Write;
    let it = &r.man.items[i];
    {
        let path = dir.join(&it.p);
        if it.d {
            return std::fs::create_dir_all(&path).is_ok();
        }
        let part = path.with_file_name(format!("{}.part", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        let Ok(mut out) = std::fs::OpenOptions::new().write(true).create_new(true).open(&part) else { return false };
        let mut pos = 0u64;
        while pos < it.s {
            // 资源管理器正在读这份清单：让它先
            while files.fetch.has_readers(&r.mid) && live() {
                std::thread::sleep(Duration::from_millis(200));
            }
            if !live() {
                return false;
            }
            let n = ((it.s - pos) as usize).min(BUF);
            match files.fetch.read(r, i as u32, pos, &mut buf[..n], true) {
                Ok(k) if k == n => {}
                Ok(_) => return false,
                Err(e) => {
                    crate::log!("文件：预取第 {i} 项失败（{e:?}），放弃预取");
                    return false;
                }
            }
            if out.write_all(&buf[..n]).is_err() {
                crate::log!("文件：写临时文件失败，放弃预取");
                return false;
            }
            pos += n as u64;
        }
        let len_ok = out.metadata().is_ok_and(|m| m.len() == it.s);
        drop(out);
        if !len_ok || std::fs::rename(&part, &path).is_err() {
            return false;
        }
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(filetime_to_system(it.m));
        }
        files.fetch.set_local(&r.mid, i as u32, path);
    }
    true
}

/// 删掉不用的临时目录（old 以前修改的，正在用的不删）。只删本程序起的名字（32 位十六进制）的真目录，
/// 不跟随联接点 / 链接。本进程启动前就有的目录按 KEEP_STARTUP 算（上次退出时剪贴板可能还指着它）。
fn clean(old: Duration) {
    let Some(base) = root() else { return };
    let Ok(rd) = std::fs::read_dir(&base) else { return };
    let keep = IN_USE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let started = STARTED.get().copied();
    for e in rd.flatten() {
        let p = e.path();
        if keep.as_ref() == Some(&p) || !ours(&e.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if !m.is_dir() || m.file_type().is_symlink() || std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x400 != 0 {
            continue;
        }
        let Ok(t) = m.modified() else { continue };
        let limit = if started.is_some_and(|s| t < s) { old.max(KEEP_STARTUP) } else { old };
        if t.elapsed().is_ok_and(|d| d > limit) {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

fn ours(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropfiles_layout() {
        let v = dropfiles(&[PathBuf::from(r"C:\a"), PathBuf::from(r"D:\文件")]);
        assert_eq!(u32::from_le_bytes(v[0..4].try_into().unwrap()), 20);
        assert_eq!(u32::from_le_bytes(v[16..20].try_into().unwrap()), 1);
        let units: Vec<u16> = v[20..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let want: Vec<u16> = r"C:\a".encode_utf16().chain([0]).chain(r"D:\文件".encode_utf16()).chain([0, 0]).collect();
        assert_eq!(units, want);
    }

    #[test]
    fn only_our_dirs() {
        assert!(ours("0123456789abcdef0123456789ABCDEF"));
        assert!(!ours("Windows"));
        assert!(!ours("0123456789abcdef0123456789abcde"));
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_system(116_444_736_000_000_000 + 10_000_000), SystemTime::UNIX_EPOCH + Duration::from_secs(1));
    }
}
