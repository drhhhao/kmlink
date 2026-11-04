//! 完整性级别（UIPI）与输入桌面检查。只在 input 线程的定时器里调用，不要在钩子回调里调用。
//!
//! - `foreground_blocked`：前台窗口所属进程的完整性级别比本进程高（例如管理员 PowerShell、任务管理器）。
//!   这时本进程（中完整性）的低级钩子收不到发往它的输入，跨屏后按键会落在本机那个窗口里。
//! - `input_desktop_default`：当前输入桌面是不是 "Default"（Ctrl+Alt+Del、UAC 安全桌面、锁屏时不是）。

use std::cell::Cell;
use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE};
use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// SECURITY_MANDATORY_HIGH_RID
const HIGH_RID: u32 = 0x3000;

/// 拿不到（拒绝访问）时当作“更高”。
enum Il {
    Rid(u32),
    Denied,
    Unknown,
}

fn token_rid(token: HANDLE) -> Option<u32> {
    unsafe {
        let mut need = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut need);
        if need == 0 || need > 4096 {
            return None;
        }
        // u64 对齐的缓冲区
        let mut buf = vec![0u64; (need as usize).div_ceil(8)];
        GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), need, &mut need).ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let n = *GetSidSubAuthorityCount(sid);
        if n == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, (n - 1) as u32))
    }
}

fn process_il(pid: u32) -> Il {
    unsafe {
        let h = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) if e.code() == E_ACCESSDENIED => return Il::Denied,
            Err(_) => return Il::Unknown,
        };
        let mut tok = HANDLE::default();
        let r = OpenProcessToken(h, TOKEN_QUERY, &mut tok);
        let _ = CloseHandle(h);
        match r {
            Ok(()) => {
                let rid = token_rid(tok);
                let _ = CloseHandle(tok);
                rid.map(Il::Rid).unwrap_or(Il::Unknown)
            }
            Err(e) if e.code() == E_ACCESSDENIED => Il::Denied,
            Err(_) => Il::Unknown,
        }
    }
}

/// 本进程的完整性 RID（算一次）。拿不到时按中完整性算。
fn own_rid() -> u32 {
    static RID: OnceLock<u32> = OnceLock::new();
    *RID.get_or_init(|| unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return 0x2000;
        }
        let r = token_rid(tok).unwrap_or(0x2000);
        let _ = CloseHandle(tok);
        r
    })
}

thread_local! {
    /// (前台 pid, 结果)：同一个进程不重复查。
    static CACHE: Cell<(u32, bool)> = const { Cell::new((0, false)) };
}

/// 前台窗口属于完整性更高的进程（本机钩子收不到它的输入）。本进程已是高完整性时永远 false。
pub fn foreground_blocked() -> bool {
    let me = own_rid();
    if me >= HIGH_RID {
        return false;
    }
    let pid = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    };
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return false;
    }
    let (cpid, cval) = CACHE.with(|c| c.get());
    if cpid == pid {
        return cval;
    }
    let blocked = match process_il(pid) {
        Il::Rid(r) => r > me,
        Il::Denied => true,
        Il::Unknown => false,
    };
    CACHE.with(|c| c.set((pid, blocked)));
    blocked
}

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> isize;
    fn GetUserObjectInformationW(h: isize, index: i32, info: *mut core::ffi::c_void, len: u32, needed: *mut u32) -> i32;
    fn CloseDesktop(h: isize) -> i32;
}

const UOI_NAME: i32 = 2;
const DESKTOP_READOBJECTS: u32 = 0x0001;

thread_local! {
    /// OpenInputDesktop 至少成功过一次：之后的失败才当作“切到了安全桌面”。
    static DESK_EVER_OK: Cell<bool> = const { Cell::new(false) };
}

/// 当前输入桌面是 "Default"。打不开时（安全桌面）返回 false；但若从没打开成功过，当作 true（环境不支持，别误判）。
pub fn input_desktop_default() -> bool {
    unsafe {
        let h = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if h == 0 {
            return !DESK_EVER_OK.with(|c| c.get());
        }
        DESK_EVER_OK.with(|c| c.set(true));
        let mut buf = [0u16; 64];
        let mut need = 0u32;
        let ok = GetUserObjectInformationW(h, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut need) != 0;
        let _ = CloseDesktop(h);
        if !ok {
            return false;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        is_default_name(&buf[..len])
    }
}

fn is_default_name(name: &[u16]) -> bool {
    String::from_utf16_lossy(name).eq_ignore_ascii_case("Default")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_name() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(is_default_name(&w("Default")));
        assert!(!is_default_name(&w("Winlogon")));
        assert!(!is_default_name(&w("")));
    }

    #[test]
    fn own_process_not_blocked() {
        // 测试进程自己的完整性一定能读到；前台查询不 panic 即可
        assert!(own_rid() >= 0x1000);
        let _ = foreground_blocked();
        let _ = input_desktop_default();
    }
}
