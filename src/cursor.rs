//! 隐藏/恢复本机系统光标（驱动对方时，本机光标停在屏幕中间，不该看见）。
//!
//! hide：对 OCR_NORMAL、OCR_IBEAM、OCR_WAIT、OCR_CROSS、OCR_UP、OCR_SIZENWSE、OCR_SIZENESW、OCR_SIZEWE、
//!       OCR_SIZENS、OCR_SIZEALL、OCR_NO、OCR_HAND、OCR_APPSTARTING、OCR_HELP(32651)、OCR_PIN(32671)、OCR_PERSON(32672)
//!       各调用 SetSystemCursor(CreateCursor(32x32 全透明：AND 掩码全 0xFF、XOR 掩码全 0), id)。
//! show：SystemParametersInfoW(SPI_SETCURSORS, 0, None, 0)（不带 SPIF_SENDCHANGE，不广播）。
//! 只在 desktop 线程调用（不要在钩子回调里调用）。
//!
//! 守护进程：主程序启动时 `spawn_guard()` 以 `kmlink.exe guard <pid>` 启动自身（CREATE_NO_WINDOW）；
//! `guard_main` OpenProcess(SYNCHRONIZE) 后 WaitForSingleObject 等主程序结束，然后 show() 恢复光标并退出。
//! 这样主程序崩溃（panic=abort）也不会把光标永久留成透明。

use windows::Win32::Foundation::{CloseHandle, HINSTANCE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, DestroyCursor, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL,
    OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, SPI_SETCURSORS, SYSTEM_CURSOR_ID,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor, SystemParametersInfoW,
};

const IDS: [SYSTEM_CURSOR_ID; 16] = [
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    OCR_HELP,
    SYSTEM_CURSOR_ID(32671), // OCR_PIN
    SYSTEM_CURSOR_ID(32672), // OCR_PERSON
];

/// 32x32 单色光标：每行 4 字节。
const MASK_LEN: usize = 32 * 32 / 8;

pub fn hide() {
    let and = [0xFFu8; MASK_LEN];
    let xor = [0u8; MASK_LEN];
    unsafe {
        let hinst = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
        let mut failed = 0;
        for id in IDS {
            // SetSystemCursor 成功后会接管（销毁）这个光标，所以每个 id 各建一个
            match CreateCursor(hinst, 0, 0, 32, 32, and.as_ptr().cast(), xor.as_ptr().cast()) {
                Ok(c) => {
                    if SetSystemCursor(c, id).is_err() {
                        let _ = DestroyCursor(c);
                        failed += 1;
                    }
                }
                Err(_) => failed += 1,
            }
        }
        if failed > 0 {
            crate::log!("隐藏光标：{failed} 个光标设置失败");
        }
    }
}

pub fn show() {
    unsafe {
        if let Err(e) = SystemParametersInfoW(SPI_SETCURSORS, 0, None, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) {
            crate::log!("恢复光标失败：{e}");
        }
    }
}

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
    unsafe {
        // 打不开（进程已经不在了）也直接恢复
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, INFINITE);
            let _ = CloseHandle(h);
        }
    }
    show();
}
