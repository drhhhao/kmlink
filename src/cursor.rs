//! 隐藏/恢复本机系统光标（驱动对方时，本机光标停在屏幕中间，不该看见）。
//!
//! 两套做法：
//! - 慢的（`hide` / `show`）：hide 对 16 个系统光标各 SetSystemCursor(新建的 32x32 全透明光标)；
//!   show = SystemParametersInfoW(SPI_SETCURSORS)（不广播），从注册表/磁盘重新加载整套光标，实测约 20ms，
//!   这段时间光标看不见/卡一下。只给守护进程、程序启动/退出、以及快速恢复不可用时兜底。
//! - 快的（光标线程，`request_hide` / `request_show`）：
//!   · 线程启动时备份：静态光标 CopyIcon 一份；动画光标（注册表里是 .ani）CopyIcon 会丢动画，
//!     只记下文件路径和当前尺寸，恢复前从文件预先加载好一份。
//!   · 隐藏用预先建好的透明光标（只剩 SetSystemCursor，最可能看见的箭头/I 形排在前面），
//!     做完后 PostThreadMessage(请求方线程, WM_APP_HIDDEN, token) —— input 线程收到后才把光标停到屏幕中间，
//!     所以跨屏时不会在屏幕中间闪一下箭头。
//!   · 恢复 = 对每个光标 SetSystemCursor(CopyIcon(备份)) / 预先加载好的动画光标，约 1~2ms。
//!   · 备份失效：注册表 HKCU\Control Panel\Cursors 的最后写入时间变了（换了光标方案/大小/主题），
//!     或 `invalidate()`（显示器变化）→ 这一次恢复走 SPI_SETCURSORS，然后重新备份。
//!   · 线程只执行积压请求里的最后一个；SetSystemCursor 会接管（销毁）传进去的光标，所以每次都给新的一份。
//! 不要在钩子回调里直接调用 hide/show；input 线程用 request_hide/request_show（不阻塞）。
//!
//! 守护进程：主程序启动时 `spawn_guard()` 以 `kmlink.exe guard <pid>` 启动自身（CREATE_NO_WINDOW）；
//! `guard_main` 把自己的工作集压到最小后等主程序结束，然后 show() 恢复光标；
//! 主程序不是正常退出（退出码 != 0：崩溃、被结束）时，再把 GetAsyncKeyState 显示还按着的
//! Shift/Ctrl/Alt（左右）、Win、鼠标键各注入一次抬起（被控端崩溃时防卡键；Alt/Win 前先注入掩码键防弹菜单）。

use crate::proto::INJECT_MARK;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HINSTANCE, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, SetProcessWorkingSetSize, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, CreateCursor, DestroyIcon, GetIconInfo, HCURSOR, HICON, ICONINFO, IMAGE_CURSOR, LR_LOADFROMFILE,
    LoadCursorW, LoadImageW, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL,
    OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, PostThreadMessageW,
    SPI_SETCURSORS, SYSTEM_CURSOR_ID, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor, SystemParametersInfoW, WM_APP,
};
use windows::core::{PCWSTR, w};

/// 光标线程隐藏完成后发给请求方线程：wParam = request_hide 的 token。
pub const WM_APP_HIDDEN: u32 = WM_APP + 3;

/// 系统光标 id 和它在 HKCU\Control Panel\Cursors 里的值名。最可能正显示着的排在前面（隐藏/恢复都按这个顺序）。
const IDS: [(SYSTEM_CURSOR_ID, &str); 16] = [
    (OCR_NORMAL, "Arrow"),
    (OCR_IBEAM, "IBeam"),
    (OCR_HAND, "Hand"),
    (OCR_SIZEWE, "SizeWE"),
    (OCR_SIZENS, "SizeNS"),
    (OCR_SIZENWSE, "SizeNWSE"),
    (OCR_SIZENESW, "SizeNESW"),
    (OCR_SIZEALL, "SizeAll"),
    (OCR_APPSTARTING, "AppStarting"),
    (OCR_WAIT, "Wait"),
    (OCR_CROSS, "Crosshair"),
    (OCR_UP, "UpArrow"),
    (OCR_NO, "No"),
    (OCR_HELP, "Help"),
    (SYSTEM_CURSOR_ID(32671), "Pin"),    // OCR_PIN
    (SYSTEM_CURSOR_ID(32672), "Person"), // OCR_PERSON
];

/// 32x32 单色光标：每行 4 字节。
const MASK_LEN: usize = 32 * 32 / 8;

const CURSORS_KEY: PCWSTR = w!("Control Panel\\Cursors");

/// 备份需要重取（显示器变化等）。
static STALE: AtomicBool = AtomicBool::new(false);

fn blank() -> Option<HCURSOR> {
    let and = [0xFFu8; MASK_LEN];
    let xor = [0u8; MASK_LEN];
    unsafe {
        let hinst = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
        CreateCursor(hinst, 0, 0, 32, 32, and.as_ptr().cast(), xor.as_ptr().cast()).ok()
    }
}

fn destroy(h: HCURSOR) {
    unsafe {
        let _ = DestroyIcon(HICON(h.0));
    }
}

/// SetSystemCursor 接管 h；失败时自己销毁。
fn set(h: HCURSOR, id: SYSTEM_CURSOR_ID) -> bool {
    if unsafe { SetSystemCursor(h, id) }.is_ok() {
        true
    } else {
        destroy(h);
        false
    }
}

/// 把所有系统光标换成透明的。`pool` 里预先建好的先用，不够再现建。
fn hide_with(pool: &mut Vec<HCURSOR>) {
    let mut failed = 0;
    for (id, _) in IDS {
        match pool.pop().or_else(blank) {
            Some(c) => {
                if !set(c, id) {
                    failed += 1;
                }
            }
            None => failed += 1,
        }
    }
    if failed > 0 {
        crate::log!("隐藏光标：{failed} 个光标设置失败");
    }
}

pub fn hide() {
    hide_with(&mut Vec::new());
}

/// 从注册表/磁盘重新加载整套系统光标（慢，约 20ms）。
pub fn show() {
    unsafe {
        if let Err(e) = SystemParametersInfoW(SPI_SETCURSORS, 0, None, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) {
            crate::log!("恢复光标失败：{e}");
        }
    }
}

/// 让光标备份失效（下次恢复走 SPI_SETCURSORS 并重新备份）。可在任意线程调用。
pub fn invalidate() {
    STALE.store(true, Ordering::Relaxed);
}

// ── 备份 ──

/// HKCU\Control Panel\Cursors 的最后写入时间（读不到为 0）。换光标方案/大小时会变。
fn reg_stamp() -> u64 {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, CURSORS_KEY, None, KEY_READ, &mut key).is_err() {
            return 0;
        }
        let mut ft = FILETIME::default();
        let r = RegQueryInfoKeyW(key, None, None, None, None, None, None, None, None, None, None, Some(&mut ft));
        let _ = RegCloseKey(key);
        if r.is_err() {
            return 0;
        }
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }
}

/// 注册表里这个光标的文件路径（已展开环境变量，带结尾 0）；没设置（用系统内置的）为 None。
fn reg_path(name: &str) -> Option<Vec<u16>> {
    let name = crate::sys::wide(name);
    let mut buf = [0u16; 260];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            CURSORS_KEY,
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if n == 0 {
        return None;
    }
    let mut v = buf[..n].to_vec();
    v.push(0);
    Some(v)
}

/// 路径（带结尾 0）是不是 .ani 动画光标。
fn is_ani(path: &[u16]) -> bool {
    let s = String::from_utf16_lossy(path);
    s.trim_end_matches('\0').to_ascii_lowercase().ends_with(".ani")
}

/// 光标当前的像素尺寸（本线程 DPI 下）。取不到为 (0, 0)（= 按系统默认尺寸加载）。
fn cursor_size(h: HCURSOR) -> (i32, i32) {
    unsafe {
        let mut ii = ICONINFO::default();
        if GetIconInfo(HICON(h.0), &mut ii).is_err() {
            return (0, 0);
        }
        let mono = ii.hbmColor.is_invalid();
        let bm = if mono { ii.hbmMask } else { ii.hbmColor };
        let mut b = BITMAP::default();
        let ok = GetObjectW(bm.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut b as *mut BITMAP).cast())) != 0;
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
        if !ok {
            return (0, 0);
        }
        // 单色光标的掩码上下两半（AND + XOR）
        (b.bmWidth, if mono { b.bmHeight / 2 } else { b.bmHeight })
    }
}

fn load_file(path: &[u16], cx: i32, cy: i32) -> Option<HCURSOR> {
    unsafe { LoadImageW(None, PCWSTR(path.as_ptr()), IMAGE_CURSOR, cx, cy, LR_LOADFROMFILE) }.ok().map(|h| HCURSOR(h.0))
}

enum Item {
    /// 静态光标的备份：恢复时 CopyIcon 一份交给 SetSystemCursor。
    Copy(HCURSOR),
    /// 动画光标：从文件加载；ready = 预先加载好的一份。
    File { path: Vec<u16>, cx: i32, cy: i32, ready: Option<HCURSOR> },
}

impl Drop for Item {
    fn drop(&mut self) {
        match self {
            Item::Copy(h) => destroy(*h),
            Item::File { ready, .. } => {
                if let Some(h) = ready.take() {
                    destroy(h);
                }
            }
        }
    }
}

/// 一整套系统光标的备份（顺序同 IDS）。
struct Saved {
    items: Vec<Item>,
    stamp: u64,
}

impl Saved {
    /// 备份此刻的系统光标（必须是正常显示的那套）。有一个取不到就整体放弃（恢复走慢的）。
    fn take() -> Option<Saved> {
        let stamp = reg_stamp();
        let mut items = Vec::with_capacity(IDS.len());
        for (id, name) in IDS {
            let cur = unsafe { LoadCursorW(None, PCWSTR(id.0 as usize as *const u16)) }.ok()?;
            let item = match reg_path(name) {
                Some(path) if is_ani(&path) => {
                    let (cx, cy) = cursor_size(cur);
                    Item::File { path, cx, cy, ready: None }
                }
                // 内置的忙/后台光标是动画的，又没有文件可加载：不做快速恢复
                None if id == OCR_WAIT || id == OCR_APPSTARTING => return None,
                _ => Item::Copy(HCURSOR(unsafe { CopyIcon(HICON(cur.0)) }.ok()?.0)),
            };
            items.push(item);
        }
        Some(Saved { items, stamp })
    }

    /// 预先加载动画光标（恢复时不用等读文件）。
    fn prepare(&mut self) {
        for it in &mut self.items {
            if let Item::File { path, cx, cy, ready } = it {
                if ready.is_none() {
                    *ready = load_file(path, *cx, *cy);
                }
            }
        }
    }

    /// 快速恢复。中途失败返回 false（调用方改走 SPI_SETCURSORS）。
    fn restore(&mut self) -> bool {
        for (it, (id, _)) in self.items.iter_mut().zip(IDS) {
            let h = match it {
                Item::Copy(b) => unsafe { CopyIcon(HICON(b.0)) }.ok().map(|h| HCURSOR(h.0)),
                Item::File { path, cx, cy, ready } => ready.take().or_else(|| load_file(path, *cx, *cy)),
            };
            match h {
                Some(h) if set(h, id) => {}
                _ => return false,
            }
        }
        true
    }
}

/// 光标线程的状态（句柄只在这个线程里用）。
struct Worker {
    saved: Option<Saved>,
    /// 预先建好的透明光标
    blanks: Vec<HCURSOR>,
}

impl Worker {
    fn new() -> Self {
        let mut w = Worker { saved: Saved::take(), blanks: Vec::with_capacity(IDS.len()) };
        if w.saved.is_none() {
            crate::log!("光标备份不可用，恢复光标走慢速路径");
        }
        w.fill_blanks();
        w
    }

    fn fill_blanks(&mut self) {
        while self.blanks.len() < IDS.len() {
            match blank() {
                Some(c) => self.blanks.push(c),
                None => break,
            }
        }
    }

    fn hide(&mut self) {
        hide_with(&mut self.blanks);
    }

    /// 隐藏之后（不赶时间）：为下一次恢复做准备。
    fn after_hide(&mut self) {
        if let Some(s) = self.saved.as_mut() {
            s.prepare();
        }
    }

    fn show(&mut self) {
        let stale = STALE.swap(false, Ordering::Relaxed);
        let fast = !stale
            && match self.saved.as_mut() {
                Some(s) => s.stamp == reg_stamp() && s.restore(),
                None => false,
            };
        if !fast {
            show();
            // 先丢掉旧备份再取新的
            self.saved = None;
            self.saved = Saved::take();
        }
    }

    /// 恢复之后：为下一次隐藏做准备。
    fn after_show(&mut self) {
        self.fill_blanks();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for h in self.blanks.drain(..) {
            destroy(h);
        }
    }
}

enum Req {
    Hide { tid: u32, token: u32 },
    Show,
}

/// 专用光标线程（第一次用时启动）。只执行最新的请求。
fn worker() -> Option<&'static crossbeam_channel::Sender<Req>> {
    static TX: std::sync::OnceLock<Option<crossbeam_channel::Sender<Req>>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<Req>();
        let r = std::thread::Builder::new().name("cursor".into()).spawn(move || {
            // 跨屏时隐藏要尽快完成（input 线程等它隐藏完才停放光标）
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }
            let mut st = Worker::new();
            while let Ok(mut req) = rx.recv() {
                // 积压的请求只看最后一个
                while let Ok(r) = rx.try_recv() {
                    req = r;
                }
                match req {
                    Req::Hide { tid, token } => {
                        st.hide();
                        notify(tid, token);
                        st.after_hide();
                    }
                    Req::Show => {
                        st.show();
                        st.after_show();
                    }
                }
            }
        });
        match r {
            Ok(_) => Some(tx),
            Err(e) => {
                crate::log!("启动光标线程失败：{e}");
                None
            }
        }
    })
    .as_ref()
}

fn notify(tid: u32, token: u32) {
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_HIDDEN, WPARAM(token as usize), LPARAM(0));
        }
    }
}

/// 预先启动光标线程并备份光标（别在钩子回调里第一次建线程）。
pub fn start_worker() {
    let _ = worker();
}

/// 请求隐藏光标（不阻塞；可在钩子回调里调用）。隐藏完成后向线程 tid 发 WM_APP_HIDDEN(token)。
pub fn request_hide(tid: u32, token: u32) {
    match worker() {
        Some(tx) => {
            let _ = tx.send(Req::Hide { tid, token });
        }
        // 没有线程：退回直接调用
        None => {
            hide();
            notify(tid, token);
        }
    }
}

/// 请求恢复光标（不阻塞；可在钩子回调里调用）。
pub fn request_show() {
    match worker() {
        Some(tx) => {
            let _ = tx.send(Req::Show);
        }
        None => show(),
    }
}

// ── 守护进程 ──

pub fn spawn_guard() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            crate::log!("启动光标守护失败：{e}");
            return;
        }
    };
    let pid = std::process::id().to_string();
    match std::process::Command::new(exe).args(["guard", &pid]).creation_flags(CREATE_NO_WINDOW).spawn() {
        Ok(_) => {} // 不等它；Child 丢掉不会结束子进程
        Err(e) => crate::log!("启动光标守护失败：{e}"),
    }
}

pub fn guard_main(pid: u32) {
    // 打不开（进程已经不在了）、取不到退出码：按异常退出处理
    let mut clean = false;
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok();
        // 守护进程只是等着：工作集压到最小（之后缺页也无所谓）
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        if let Some(h) = h {
            let _ = WaitForSingleObject(h, INFINITE);
            let mut code = 1u32;
            clean = GetExitCodeProcess(h, &mut code).is_ok() && code == 0;
            let _ = CloseHandle(h);
        }
    }
    show();
    if !clean {
        let v = stuck_inputs(|vk| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0);
        if !v.is_empty() {
            unsafe {
                let _ = SendInput(&v, std::mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

/// 守护进程检查的修饰键（LShift RShift LCtrl RCtrl LAlt RAlt LWin RWin）。
const GUARD_KEYS: [u16; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];
/// 扩展键：RCtrl RAlt LWin RWin。
const EXT_KEYS: [u16; 4] = [0xA3, 0xA5, 0x5B, 0x5C];
/// Alt/Win：单独抬起会弹菜单栏/开始菜单，先注入掩码键。
const MENU_KEYS: [u16; 4] = [0xA4, 0xA5, 0x5B, 0x5C];
/// 未分配的虚拟键（同 AutoHotkey 的 vkE8）。
const VK_MASK: u16 = 0xE8;

fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK } },
    }
}

/// 还按着（down(vk) 为真）的修饰键、Win 键、鼠标键各一次抬起；有 Alt/Win 时最前面加掩码键按下抬起。
fn stuck_inputs(down: impl Fn(u16) -> bool) -> Vec<INPUT> {
    let keys: Vec<u16> = GUARD_KEYS.iter().copied().filter(|&k| down(k)).collect();
    let mut v = Vec::new();
    if keys.iter().any(|k| MENU_KEYS.contains(k)) {
        v.push(key_input(VK_MASK, 0, KEYBD_EVENT_FLAGS(0)));
        v.push(key_input(VK_MASK, 0, KEYEVENTF_KEYUP));
    }
    for k in keys {
        let scan = unsafe { MapVirtualKeyW(k as u32, MAPVK_VK_TO_VSC) } as u16;
        let flags = if EXT_KEYS.contains(&k) { KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY } else { KEYEVENTF_KEYUP };
        v.push(key_input(k, scan, flags));
    }
    let buttons = [
        (0x01, MOUSEEVENTF_LEFTUP, 0),
        (0x02, MOUSEEVENTF_RIGHTUP, 0),
        (0x04, MOUSEEVENTF_MIDDLEUP, 0),
        (0x05, MOUSEEVENTF_XUP, 1),
        (0x06, MOUSEEVENTF_XUP, 2),
    ];
    for (vk, flags, data) in buttons {
        if down(vk) {
            v.push(mouse_input(flags, data));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_keys_released_with_mask() {
        assert!(stuck_inputs(|_| false).is_empty());
        // Shift + 左键：不需要掩码
        let v = stuck_inputs(|vk| vk == 0xA0 || vk == 0x01);
        assert_eq!(v.len(), 2);
        unsafe {
            assert_eq!(v[0].Anonymous.ki.wVk, VIRTUAL_KEY(0xA0));
            assert_eq!(v[0].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
            assert_eq!(v[1].Anonymous.mi.dwFlags, MOUSEEVENTF_LEFTUP);
        }
        // Win + X2：先掩码键按下抬起，再抬 Win（扩展键），再抬 X2
        let v = stuck_inputs(|vk| vk == 0x5B || vk == 0x06);
        assert_eq!(v.len(), 4);
        unsafe {
            assert_eq!(v[0].Anonymous.ki.wVk, VIRTUAL_KEY(VK_MASK));
            assert_eq!(v[0].Anonymous.ki.dwFlags, KEYBD_EVENT_FLAGS(0));
            assert_eq!(v[1].Anonymous.ki.wVk, VIRTUAL_KEY(VK_MASK));
            assert_eq!(v[1].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
            assert_eq!(v[2].Anonymous.ki.wVk, VIRTUAL_KEY(0x5B));
            assert_eq!(v[2].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY);
            assert_eq!(v[3].Anonymous.mi.dwFlags, MOUSEEVENTF_XUP);
            assert_eq!(v[3].Anonymous.mi.mouseData, 2);
        }
    }

    #[test]
    fn ani_detection() {
        let w = |s: &str| crate::sys::wide(s);
        assert!(is_ani(&w("C:\\Windows\\Cursors\\aero_busy.ANI")));
        assert!(!is_ani(&w("C:\\Windows\\Cursors\\aero_arrow.cur")));
    }

    /// 真的隐藏再快速恢复一次（光标会消失几毫秒），检查恢复后是原来那套、动画光标还在动，并打印耗时。
    /// 手动跑：cargo test cursor::tests::real_hide_and_fast_restore -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_hide_and_fast_restore() {
        type FrameInfo = unsafe extern "system" fn(HCURSOR, u32, u32, *mut u32, *mut u32) -> HCURSOR;
        let frames = |id: SYSTEM_CURSOR_ID| -> u32 {
            unsafe {
                let m = windows::Win32::System::LibraryLoader::GetModuleHandleW(w!("user32.dll")).unwrap();
                let Some(f) = windows::Win32::System::LibraryLoader::GetProcAddress(m, windows::core::s!("GetCursorFrameInfo")) else {
                    return 0;
                };
                let f: FrameInfo = std::mem::transmute(f);
                let h = LoadCursorW(None, PCWSTR(id.0 as usize as *const u16)).unwrap();
                let (mut rate, mut steps) = (0, 0);
                f(h, 0, 0, &mut rate, &mut steps);
                steps
            }
        };
        let arrow = || unsafe { cursor_size(LoadCursorW(None, PCWSTR(OCR_NORMAL.0 as usize as *const u16)).unwrap()) };
        let before = (arrow(), frames(OCR_WAIT));
        let mut w = Worker::new();
        assert!(w.saved.is_some(), "备份不可用");
        let t = std::time::Instant::now();
        w.hide();
        let hide_ms = t.elapsed().as_secs_f64() * 1000.0;
        w.after_hide();
        let t = std::time::Instant::now();
        w.show();
        let show_ms = t.elapsed().as_secs_f64() * 1000.0;
        let after = (arrow(), frames(OCR_WAIT));
        w.after_show();
        let t = std::time::Instant::now();
        show();
        let spi_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("隐藏 {hide_ms:.2}ms，快速恢复 {show_ms:.2}ms，SPI_SETCURSORS {spi_ms:.2}ms；恢复前后 {before:?} / {after:?}");
        assert_eq!(before, after);
    }

    #[test]
    fn backup_reads_without_touching_cursors() {
        // 只读：取一份备份、预加载动画光标，然后全部释放（不调用 SetSystemCursor）
        let _ = reg_stamp();
        if let Some(mut s) = Saved::take() {
            assert_eq!(s.items.len(), IDS.len());
            s.prepare();
        }
    }
}
