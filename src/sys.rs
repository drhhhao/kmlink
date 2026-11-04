//! 少量公用的 Win32 小工具。

use crate::geom::{Desk, Rect};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::BOOL;

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

/// UTF-16 + 结尾 0。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
