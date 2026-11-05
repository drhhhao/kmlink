//! 少量公用的 Win32 小工具。

use crate::geom::{Desk, Rect};
use std::path::Path;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE,
    ERROR_SUCCESS, GetLastError, LPARAM, RECT, WIN32_ERROR,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::core::{BOOL, PCWSTR, w};

/// 主程序的单实例互斥量（main.rs 建，设置窗口拿来判断主程序在不在）。
pub const DAEMON_MUTEX: PCWSTR = w!("Local\\kmlink-daemon");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("kmlink");

/// 必须在创建任何窗口之前调用：之后所有坐标都是物理像素。
pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 本机所有显示器（物理像素，虚拟桌面坐标），按 (左, 上) 排序。
pub fn monitors() -> Desk {
    monitors_dpi().0
}

/// 本机显示器 + 每块的有效 DPI（96 = 100%，和 monitors 一一对应，顺序同 `monitors()`）。取不到 DPI 的按 96。
pub fn monitors_dpi() -> (Desk, Vec<u32>) {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let v = &mut *(data.0 as *mut Vec<(Rect, u32)>);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(h, &mut mi).as_bool() {
                let r = mi.rcMonitor;
                let (mut dx, mut dy) = (0u32, 0u32);
                let dpi = match GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) {
                    Ok(()) if dx > 0 => dx,
                    _ => 96,
                };
                v.push((Rect::new(r.left, r.top, r.right, r.bottom), dpi));
            }
        }
        BOOL(1)
    }
    let mut v: Vec<(Rect, u32)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.sort_by_key(|(r, _)| (r.l, r.t));
    let (rects, dpis) = v.into_iter().unzip();
    (Desk::new(rects), dpis)
}

/// 进程优先级提到“高于正常”。计划任务启动的进程默认是“低于正常”，对方电脑一忙，
/// 收包、注入鼠标的线程就排在普通程序后面（实测“在另一台上操作时卡”）。
pub fn raise_process_priority() {
    use windows::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass};
    if let Err(e) = unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) } {
        crate::log!("提高进程优先级失败：{e}");
    }
}

/// 当前线程的优先级：critical = 最高（只给几乎总在等、一来活就要立刻处理的线程），否则 HIGHEST。
pub fn boost_thread(critical: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL,
    };
    let p = if critical { THREAD_PRIORITY_TIME_CRITICAL } else { THREAD_PRIORITY_HIGHEST };
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), p) } {
        crate::log!("设置线程优先级失败：{e}");
    }
}

/// UTF-16 + 结尾 0。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 单实例：创建命名互斥量 name，已有同名的返回 true（本进程该退出）。
/// 另一个实例以管理员身份运行时，普通权限打开它会被拒绝访问（ERROR_ACCESS_DENIED），也算“已在运行”。
/// 句柄不关，进程结束时系统回收。
pub fn already_running(name: PCWSTR, own: bool) -> bool {
    let (created, err) = unsafe {
        match CreateMutexW(None, own, name) {
            Ok(_) => (true, GetLastError()),
            Err(e) => (false, WIN32_ERROR::from_error(&e).unwrap_or(ERROR_INVALID_HANDLE)),
        }
    };
    if !created && err != ERROR_ACCESS_DENIED {
        crate::log!("创建互斥量失败：{}", err.0);
    }
    mutex_taken(created, err)
}

/// 互斥量是否已被别的实例占着：创建成功但已存在，或者没权限打开。
fn mutex_taken(created: bool, err: WIN32_ERROR) -> bool {
    if created { err == ERROR_ALREADY_EXISTS } else { err == ERROR_ACCESS_DENIED }
}

/// 命名互斥量现在有没有（只打开、不创建）。别的实例以管理员身份运行时打开会被拒绝，也算有。
fn mutex_exists(name: PCWSTR) -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, name) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(e) => e.code() == E_ACCESSDENIED,
        }
    }
}

/// 主程序（守护进程）在不在运行：看它的单实例互斥量。
pub fn daemon_running() -> bool {
    mutex_exists(DAEMON_MUTEX)
}

// ---------- 开机自启（HKCU\...\Run 的 "kmlink" 值） ----------

/// 开机自启的注册表值：带引号的 exe 路径。
pub(crate) fn run_value(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn open_run_key(sam: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut k = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, sam, &mut k) };
    (r == ERROR_SUCCESS).then_some(k)
}

/// 当前 Run 里 "kmlink" 的值（没有则 None）。
pub(crate) fn autostart_get() -> Option<String> {
    let k = open_run_key(KEY_QUERY_VALUE)?;
    let mut out = None;
    unsafe {
        let mut len = 0u32;
        if RegQueryValueExW(k, RUN_VALUE, None, None, None, Some(&mut len)) == ERROR_SUCCESS {
            let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
            let mut bytes = (buf.len() * 2) as u32;
            if RegQueryValueExW(k, RUN_VALUE, None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut bytes))
                == ERROR_SUCCESS
            {
                let n = (bytes as usize / 2).min(buf.len());
                out = Some(crate::clip::utf16_until_nul(&buf[..n]));
            }
        }
        let _ = RegCloseKey(k);
    }
    out
}

/// 写/删 Run 值（写的是当前 exe 的路径）。成功返回 true。
pub(crate) fn autostart_set(on: bool) -> bool {
    let Some(k) = open_run_key(KEY_SET_VALUE) else {
        crate::log!("开机自启：打开注册表失败");
        return false;
    };
    let ok = unsafe {
        if on {
            match std::env::current_exe() {
                Ok(exe) => {
                    let bytes: Vec<u8> = wide(&run_value(&exe)).iter().flat_map(|u| u.to_le_bytes()).collect();
                    RegSetValueExW(k, RUN_VALUE, None, REG_SZ, Some(&bytes)) == ERROR_SUCCESS
                }
                Err(e) => {
                    crate::log!("开机自启：取 exe 路径失败：{e}");
                    false
                }
            }
        } else {
            let r = RegDeleteValueW(k, RUN_VALUE);
            r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND
        }
    };
    unsafe {
        let _ = RegCloseKey(k);
    }
    if !ok {
        crate::log!("开机自启：{}失败", if on { "写入" } else { "删除" });
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_rules() {
        assert!(!mutex_taken(true, ERROR_SUCCESS));
        assert!(mutex_taken(true, ERROR_ALREADY_EXISTS));
        assert!(mutex_taken(false, ERROR_ACCESS_DENIED));
        // 其它失败：照常运行
        assert!(!mutex_taken(false, ERROR_INVALID_HANDLE));
        // 真实互斥量：同一进程再建一次也是“已存在”
        let name = wide(&format!("Local\\kmlink-test-{}", std::process::id()));
        assert!(!already_running(PCWSTR(name.as_ptr()), false));
        assert!(already_running(PCWSTR(name.as_ptr()), false));
    }

    #[test]
    fn mutex_probe() {
        let name = wide(&format!("Local\\kmlink-probe-{}", std::process::id()));
        assert!(!mutex_exists(PCWSTR(name.as_ptr())));
        // 只打开不创建：探测本身不会让它存在
        assert!(!mutex_exists(PCWSTR(name.as_ptr())));
        assert!(!already_running(PCWSTR(name.as_ptr()), false));
        assert!(mutex_exists(PCWSTR(name.as_ptr())));
    }

    #[test]
    fn run_value_quoted() {
        let p = Path::new(r"C:\Program Files\kmlink\kmlink.exe");
        assert_eq!(run_value(p), "\"C:\\Program Files\\kmlink\\kmlink.exe\"");
    }

    #[test]
    fn monitors_with_dpi() {
        let (d, dpis) = monitors_dpi();
        assert_eq!(d, monitors());
        assert_eq!(dpis.len(), d.monitors.len());
        assert!(dpis.iter().all(|&x| x > 0));
    }
}
