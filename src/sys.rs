//! 少量公用的 Win32 小工具。

use crate::geom::{Desk, Rect};
#[cfg(test)]
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INVALID_HANDLE, GetLastError, LPARAM, RECT, WIN32_ERROR,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::{BOOL, PCWSTR};

/// 必须在创建任何窗口之前调用：之后所有坐标都是物理像素。
pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 本机所有显示器（物理像素，虚拟桌面坐标）。
pub fn monitors() -> Desk {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let v = &mut *(data.0 as *mut Vec<Rect>);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(h, &mut mi).as_bool() {
                let r = mi.rcMonitor;
                v.push(Rect::new(r.left, r.top, r.right, r.bottom));
            }
        }
        BOOL(1)
    }
    let mut v: Vec<Rect> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.sort_by_key(|r| (r.l, r.t));
    Desk::new(v)
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
}
