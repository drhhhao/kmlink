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

pub fn hide() {
    todo!()
}

pub fn show() {
    todo!()
}

pub fn spawn_guard() {
    todo!()
}

pub fn guard_main(pid: u32) {
    let _ = pid;
    todo!()
}
